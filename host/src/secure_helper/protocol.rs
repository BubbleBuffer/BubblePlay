//! Bounded, versioned control protocol; pixel data never travels in the pipe.
pub use crate::admin_agent::protocol::Role;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
pub const PROTOCOL_MIN: u16 = 2;
pub const PROTOCOL_MAX: u16 = 2;
pub const MAGIC: &[u8; 4] = b"BPSH";
pub const HEADER_LEN: usize = 8;
pub const MAX_PAYLOAD: usize = 24 * 1024;
pub const SLOT_BYTES: usize = 64 * 1024 * 1024;
pub const FRAME_HEADER: usize = 40;
pub const SLOT_SPAN: usize = FRAME_HEADER + SLOT_BYTES;
pub const MAPPING_BYTES: usize = 2 * SLOT_SPAN;
pub fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\BubblePlay.SecureHelper.s{session}")
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}
impl Region {
    pub fn validate(self) -> io::Result<()> {
        frame_len(
            self.width,
            self.height,
            self.width.checked_mul(4).ok_or_else(bad)?,
        )?;
        self.x.checked_add_unsigned(self.width).ok_or_else(bad)?;
        self.y.checked_add_unsigned(self.height).ok_or_else(bad)?;
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Uac,
    Lock,
    Other,
}
impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Uac => "uac",
            Self::Lock => "lock",
            Self::Other => "other",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Message {
    Hello {
        min: u16,
        max: u16,
        role: u8,
    },
    Welcome {
        version: u16,
        min: u16,
        max: u16,
        pid: u32,
    },
    Incompatible {
        min: u16,
        max: u16,
    },
    Poll {
        region: Region,
    },
    Input {
        id: u64,
        records: Vec<u8>,
        epoch: u64,
    },
    DenyConsent {
        epoch: u64,
        id: u64,
    },
    Ack {
        slot: u8,
        seq: u64,
    },
    State {
        secure: bool,
        kind: Kind,
        admin: bool,
        available: bool,
        epoch: u64,
    },
    Mapping {
        name: String,
    },
    Frame {
        input_id: u64,
        slot: u8,
        seq: u64,
        width: u32,
        height: u32,
        stride: u32,
        hash: u64,
        epoch: u64,
    },
    Denied {
        id: u64,
        ok: bool,
        reason: String,
    },
    Refused,
    Busy,
    InputReceipt(InputReceipt),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputReceipt {
    pub id: u64,
    pub received: u16,
    pub accepted: u16,
    pub applied: u16,
    pub error_code: u32,
    pub reason: String,
}
impl InputReceipt {
    pub fn rejected_after_policy(id: u64, received: u16, reason: &'static str) -> Self {
        let mut r = Self::rejected(id, received, reason);
        r.accepted = received;
        r
    }
    pub fn rejected(id: u64, received: u16, reason: &'static str) -> Self {
        Self {
            id,
            received,
            accepted: 0,
            applied: 0,
            error_code: 0,
            reason: reason.into(),
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        if self.id == 0
            || self.received > 1024
            || self.accepted > self.received
            || self.applied > self.accepted
            || self.reason.len() > 64
            || reason_name(&self.reason) == "unknown"
        {
            return Err(bad());
        }
        Ok(())
    }
}
/// Accepted counts valid current-epoch records admitted by Admin mode, even
/// if a later worker/geometry/Windows failure prevents their application.
pub struct InputGate {
    pub last_id: u64,
    pub epoch: u64,
    pub policy_on: bool,
    pub worker_ready: bool,
    pub outstanding: usize,
    pub rate_allowed: bool,
}
impl InputGate {
    pub fn rejection(&self, id: u64, target: u64, valid: bool) -> Option<(&'static str, bool)> {
        if id == 0 || id <= self.last_id {
            Some(("stale-id", false))
        } else if !valid {
            Some(("invalid-records", false))
        } else if target != self.epoch {
            Some(("stale-epoch", false))
        } else if !self.policy_on {
            Some(("policy-off", false))
        } else if !self.rate_allowed {
            Some(("server-rate-limit", true))
        } else if !self.worker_ready {
            Some(("worker-missing", true))
        } else if self.outstanding >= 32 {
            Some(("worker-full", true))
        } else {
            None
        }
    }
}
pub fn reason_name(reason: &str) -> &'static str {
    match reason {
        "ok" => "ok",
        "partial-send-input" => "partial-send-input",
        "invalid-records" => "invalid-records",
        "stale-id" => "stale-id",
        "stale-epoch" => "stale-epoch",
        "worker-missing" => "worker-missing",
        "worker-stopped" => "worker-stopped",
        "worker-full" => "worker-full",
        "server-rate-limit" => "server-rate-limit",
        "input-rate-limit" => "input-rate-limit",
        "policy-off" => "policy-off",
        "capture-unavailable" => "capture-unavailable",
        "desktop-changed" => "desktop-changed",
        "invalid-geometry" => "invalid-geometry",
        "no_input_capability" => "no_input_capability",
        "not_secure" => "not_secure",
        "not_ready" => "not_ready",
        "queue-full" => "queue-full",
        "client-invalid" => "client-invalid",
        "link-lost" => "link-lost",
        "receipt-overflow" => "receipt-overflow",
        _ => "unknown",
    }
}
/// Wire v2 absolute input is normalized to the requested capture rectangle.
/// Convert using metrics sampled on the bound desktop, never the host's desktop.
pub fn map_absolute(
    record: crate::admin_agent::protocol::Record,
    region: Region,
    virtual_screen: Region,
) -> io::Result<crate::admin_agent::protocol::Record> {
    use crate::admin_agent::protocol::{Record, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_VIRTUALDESK};
    record.validate().map_err(|_| bad())?;
    let Record::Mouse {
        dx,
        dy,
        data,
        flags,
    } = record
    else {
        return Ok(record);
    };
    if flags & MOUSEEVENTF_ABSOLUTE == 0 {
        return Ok(record);
    }
    region.validate()?;
    if virtual_screen.width == 0
        || virtual_screen.height == 0
        || virtual_screen.width > 1_048_576
        || virtual_screen.height > 1_048_576
    {
        return Err(bad());
    }
    let axis = |v: i32, origin: i32, size: u32, base: i32, total: u32| -> io::Result<i32> {
        let pixel =
            i64::from(origin) + (i64::from(v) * i64::from(size.saturating_sub(1)) + 32767) / 65535;
        let offset = pixel - i64::from(base);
        if offset < 0 || offset >= i64::from(total) {
            return Err(bad());
        }
        Ok(if total == 1 {
            0
        } else {
            ((offset * 65535 + i64::from(total - 1) / 2) / i64::from(total - 1)) as i32
        })
    };
    Ok(Record::Mouse {
        dx: axis(
            dx,
            region.x,
            region.width,
            virtual_screen.x,
            virtual_screen.width,
        )?,
        dy: axis(
            dy,
            region.y,
            region.height,
            virtual_screen.y,
            virtual_screen.height,
        )?,
        data,
        flags: flags | MOUSEEVENTF_VIRTUALDESK,
    })
}
pub fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid secure-helper message")
}
pub fn encode(m: &Message) -> io::Result<Vec<u8>> {
    let p = serde_json::to_vec(m).map_err(|_| bad())?;
    if p.len() > MAX_PAYLOAD {
        return Err(bad());
    }
    let mut b = Vec::with_capacity(HEADER_LEN + p.len());
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&(p.len() as u32).to_le_bytes());
    b.extend(p);
    Ok(b)
}
pub fn payload_len(header: &[u8]) -> io::Result<usize> {
    if header.len() != HEADER_LEN || &header[..4] != MAGIC {
        return Err(bad());
    }
    let n = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if n == 0 || n > MAX_PAYLOAD {
        return Err(bad());
    }
    Ok(n)
}
pub fn read(r: &mut impl Read) -> io::Result<Message> {
    let mut h = [0; HEADER_LEN];
    r.read_exact(&mut h)?;
    let n = payload_len(&h)?;
    let mut p = vec![0; n];
    r.read_exact(&mut p)?;
    serde_json::from_slice(&p).map_err(|_| bad())
}
pub fn write(w: &mut impl Write, m: &Message) -> io::Result<()> {
    w.write_all(&encode(m)?)
}
pub fn frame_len(width: u32, height: u32, stride: u32) -> io::Result<usize> {
    if width == 0
        || height == 0
        || width > 8192
        || height > 8192
        || stride != width.checked_mul(4).ok_or_else(bad)?
    {
        return Err(bad());
    }
    let len = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(bad)?;
    if len > SLOT_BYTES {
        return Err(bad());
    }
    Ok(len)
}
pub fn hash(p: &[u8]) -> u64 {
    p.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub slot: u8,
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub hash: u64,
    pub epoch: u64,
}
impl FrameInfo {
    pub fn validate(self, last: u64) -> io::Result<usize> {
        if self.slot > 1
            || self.seq == 0
            || self.seq <= last
            || self.seq == u64::MAX
            || self.epoch == 0
        {
            return Err(bad());
        }
        frame_len(self.width, self.height, self.stride)
    }
    pub fn header(self) -> [u8; FRAME_HEADER] {
        let mut h = [0; FRAME_HEADER];
        h[..4].copy_from_slice(b"BPSF");
        h[4..8].copy_from_slice(&(FRAME_HEADER as u32).to_le_bytes());
        h[8..16].copy_from_slice(&self.seq.to_le_bytes());
        h[16..20].copy_from_slice(&self.width.to_le_bytes());
        h[20..24].copy_from_slice(&self.height.to_le_bytes());
        h[24..28].copy_from_slice(&self.stride.to_le_bytes());
        h[28..32]
            .copy_from_slice(&((self.stride as u64 * self.height as u64) as u32).to_le_bytes());
        h[32..40].copy_from_slice(&self.hash.to_le_bytes());
        h
    }
    pub fn validate_copy(self, header: &[u8], pixels: &[u8], last: u64) -> io::Result<()> {
        let len = self.validate(last)?;
        if header != self.header() || pixels.len() != len || hash(pixels) != self.hash {
            return Err(bad());
        }
        Ok(())
    }
}
pub fn consent_matches(image: &str, session: Option<u32>, expected: &str, console: u32) -> bool {
    session == Some(console) && image.eq_ignore_ascii_case(expected)
}
pub fn caps(secure: bool, kind: Kind, admin: bool, available: bool) -> Vec<&'static str> {
    if !secure || !available {
        return Vec::new();
    }
    let mut c = vec!["view"];
    if kind == Kind::Uac {
        c.push("deny");
    }
    if admin {
        c.push("input");
    }
    c
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelloResult {
    Accepted { version: u16, role: Role },
    Incompatible,
    Invalid,
}
pub fn hello(message: &Message) -> HelloResult {
    let Message::Hello { min, max, role } = *message else {
        return HelloResult::Invalid;
    };
    let role = match role {
        1 => Role::Host,
        2 => Role::Probe,
        _ => return HelloResult::Invalid,
    };
    match crate::admin_agent::protocol::negotiate(min, max, PROTOCOL_MIN, PROTOCOL_MAX) {
        Some(version) => HelloResult::Accepted { version, role },
        None => HelloResult::Incompatible,
    }
}
pub fn allows_input(secure: bool, admin: bool, available: bool) -> bool {
    secure && admin && available
}
pub fn allows_deny(secure: bool, kind: Kind) -> bool {
    secure && kind == Kind::Uac
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hello_and_bounds() {
        let m = Message::Hello {
            min: 1,
            max: 1,
            role: 1,
        };
        let b = encode(&m).unwrap();
        assert_eq!(read(&mut b.as_slice()).unwrap(), m);
        assert!(payload_len(b"BPSH\xff\xff\xff\xff").is_err());
        assert!(payload_len(b"BPSH\0\0\0\0").is_err());
        assert!(read(&mut &b"BPSH\x01\0\0\0{"[..]).is_err());
        assert_eq!(crate::admin_agent::protocol::negotiate(2, 3, 1, 1), None);
        assert!(serde_json::from_str::<Message>(
            r#"{"Hello":{"min":1,"max":1,"role":1,"extra":true}}"#
        )
        .is_err());
    }
    #[test]
    fn malicious_frame_headers() {
        let p = vec![42; 16];
        let f = FrameInfo {
            slot: 0,
            seq: 1,
            width: 2,
            height: 2,
            stride: 8,
            hash: hash(&p),
            epoch: 1,
        };
        assert!(f.validate_copy(&f.header(), &p, 0).is_ok());
        assert!(f.validate(1).is_err());
        for bad_f in [
            FrameInfo { slot: 2, ..f },
            FrameInfo { seq: 0, ..f },
            FrameInfo { stride: 12, ..f },
            FrameInfo {
                width: u32::MAX,
                ..f
            },
            FrameInfo {
                height: 8192,
                width: 8192,
                stride: 32768,
                ..f
            },
            FrameInfo { epoch: 0, ..f },
        ] {
            assert!(bad_f.validate(0).is_err());
        }
        let mut h = f.header();
        h[28] = 255;
        assert!(f.validate_copy(&h, &p, 0).is_err());
        let mut torn = p;
        torn[1] = 0;
        assert!(f.validate_copy(&f.header(), &torn, 0).is_err());
    }
    #[test]
    fn consent_identity_and_policy() {
        let e = r"C:\Windows\System32\consent.exe";
        assert!(consent_matches(e, Some(2), e, 2));
        assert!(!consent_matches(e, Some(3), e, 2));
        assert!(!consent_matches(r"C:\Temp\consent.exe", Some(2), e, 2));
        assert!(!consent_matches(
            r"C:\Windows\System32\consent.exe.fake",
            Some(2),
            e,
            2
        ));
        assert_eq!(caps(true, Kind::Uac, false, true), vec!["view", "deny"]);
        assert_eq!(caps(true, Kind::Lock, true, true), vec!["view", "input"]);
        assert!(caps(false, Kind::Uac, true, true).is_empty());
    }
    #[test]
    fn policy_matrix() {
        for kind in [Kind::Uac, Kind::Lock, Kind::Other] {
            for admin in [false, true] {
                let c = caps(true, kind, admin, true);
                assert!(c.contains(&"view"));
                assert_eq!(c.contains(&"input"), admin);
                assert_eq!(c.contains(&"deny"), kind == Kind::Uac);
                assert_eq!(allows_input(true, admin, true), admin);
                assert_eq!(allows_deny(true, kind), kind == Kind::Uac);
                assert!(!allows_input(false, admin, true));
                assert!(!allows_input(true, admin, false));
                assert!(!allows_deny(false, kind));
                assert!(caps(true, kind, admin, false).is_empty());
            }
        }
    }
    #[test]
    fn hello_gate_requires_first_hello_and_known_role() {
        assert_eq!(
            hello(&Message::Hello {
                min: 2,
                max: 2,
                role: 1
            }),
            HelloResult::Accepted {
                version: 2,
                role: Role::Host
            }
        );
        assert_eq!(
            hello(&Message::Hello {
                min: 2,
                max: 2,
                role: 2
            }),
            HelloResult::Accepted {
                version: 2,
                role: Role::Probe
            }
        );
        assert_eq!(
            hello(&Message::Hello {
                min: 1,
                max: 1,
                role: 1
            }),
            HelloResult::Incompatible
        );
        assert_eq!(
            hello(&Message::Hello {
                min: 3,
                max: 1,
                role: 1
            }),
            HelloResult::Incompatible
        );
        assert_eq!(
            hello(&Message::Hello {
                min: 1,
                max: 1,
                role: 3
            }),
            HelloResult::Invalid
        );
        assert_eq!(
            hello(&Message::DenyConsent { epoch: 1, id: 1 }),
            HelloResult::Invalid
        );
    }
    #[test]
    fn absolute_mapping_uses_bound_desktop_geometry() {
        use crate::admin_agent::protocol::{
            Record, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
        };
        let record = |x, y| Record::Mouse {
            dx: x,
            dy: y,
            data: 0,
            flags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE,
        };
        let selected = Region {
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let secure = Region {
            x: -1920,
            y: 0,
            width: 3840,
            height: 1080,
        };
        assert_eq!(
            map_absolute(record(0, 0), selected, secure).unwrap(),
            Record::Mouse {
                dx: 0,
                dy: 0,
                data: 0,
                flags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_MOVE | MOUSEEVENTF_VIRTUALDESK
            }
        );
        let Record::Mouse { dx, dy, .. } =
            map_absolute(record(65535, 65535), selected, secure).unwrap()
        else {
            panic!()
        };
        assert!(dx > 32750 && dx < 32768);
        assert_eq!(dy, 65535);
        let Record::Mouse { dx, .. } =
            map_absolute(record(32768, 32768), selected, secure).unwrap()
        else {
            panic!()
        };
        assert!((16000..17000).contains(&dx));
        assert!(map_absolute(record(65536, 0), selected, secure).is_err());
        assert!(map_absolute(record(0, 0), selected, Region { x: 0, ..secure }).is_err());
        assert!(map_absolute(record(0, 0), selected, Region { width: 0, ..secure }).is_err());
    }
    #[test]
    fn receipts_roundtrip_and_bounds() {
        let r = InputReceipt {
            id: 1,
            received: 2,
            accepted: 2,
            applied: 1,
            error_code: 5,
            reason: "partial-send-input".into(),
        };
        assert!(r.validate().is_ok());
        let m = Message::InputReceipt(r.clone());
        assert_eq!(read(&mut encode(&m).unwrap().as_slice()).unwrap(), m);
        for bad_r in [
            InputReceipt {
                applied: 3,
                ..r.clone()
            },
            InputReceipt {
                accepted: 3,
                ..r.clone()
            },
            InputReceipt {
                reason: "private-key-values".into(),
                ..r.clone()
            },
            InputReceipt {
                received: 1025,
                ..r.clone()
            },
        ] {
            assert!(bad_r.validate().is_err());
        }
        assert!(InputReceipt::rejected(1, 2, "policy-off")
            .validate()
            .is_ok());
        assert_eq!(
            read(&mut encode(&Message::Busy).unwrap().as_slice()).unwrap(),
            Message::Busy
        );
    }
    #[test]
    fn admission_receipts_distinguish_policy_from_application_failures() {
        let mut gate = InputGate {
            last_id: 0,
            epoch: 1,
            policy_on: false,
            worker_ready: true,
            outstanding: 0,
            rate_allowed: true,
        };
        assert_eq!(gate.rejection(1, 1, true), Some(("policy-off", false)));
        gate.policy_on = true;
        assert_eq!(gate.rejection(1, 1, true), None);
        assert_eq!(gate.rejection(1, 2, true), Some(("stale-epoch", false)));
        assert_eq!(
            gate.rejection(1, 1, false),
            Some(("invalid-records", false))
        );
        gate.last_id = 1;
        assert_eq!(gate.rejection(1, 1, true), Some(("stale-id", false)));
        gate.last_id = 0;
        gate.worker_ready = false;
        assert_eq!(gate.rejection(1, 1, true), Some(("worker-missing", true)));
        gate.worker_ready = true;
        gate.outstanding = 32;
        assert_eq!(gate.rejection(1, 1, true), Some(("worker-full", true)));
        gate.outstanding = 0;
        gate.rate_allowed = false;
        assert_eq!(
            gate.rejection(1, 1, true),
            Some(("server-rate-limit", true))
        );
        let r = InputReceipt::rejected_after_policy(1, 2, "invalid-geometry");
        assert_eq!((r.received, r.accepted, r.applied), (2, 2, 0));
    }
}
