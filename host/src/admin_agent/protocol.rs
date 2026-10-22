//! Wire protocol between the user-level host (or tray) and the elevated
//! admin input agent.
//!
//! The agent is a thin `SendInput` relay. The host keeps all browser-packet
//! parsing, key/button state, cursor mapping and clipboard handling and sends
//! only plain Windows `INPUT` records (mouse or keyboard). Ordinary host
//! changes therefore never need an agent change; only a new kind of Windows
//! input primitive would.
//!
//! Framing is magic, then a length-prefixed payload. It deliberately does not
//! carry whole browser packets, which would tie the agent to the host's input
//! format.
//!
//! Frozen across every protocol version, so any agent can tell any client
//! which versions it speaks:
//! - frame header: magic `BPAG`, kind `u8`, reserved `u8` (0), payload
//!   length `u16` little endian;
//! - `Hello` (kind 1): min `u16`, max `u16`, role `u8`;
//! - `Welcome` (kind 0x81) and `Refused` (kind 0x83) payloads.
//!
//! The first client message must be `Hello`; the agent answers `Welcome`
//! with the negotiated version or `Refused { code: Version }` with its range.

use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"BPAG";
pub const HEADER_LEN: usize = 8;
/// Protocol versions this build speaks (host side and agent side).
pub const PROTOCOL_MIN: u16 = 1;
pub const PROTOCOL_MAX: u16 = 1;
/// Records per `Inject` frame; the host splits larger batches.
pub const MAX_RECORDS_PER_FRAME: usize = 1024;
pub const RECORD_LEN: usize = 17;
pub const MAX_PAYLOAD: usize = 2 + MAX_RECORDS_PER_FRAME * RECORD_LEN;

pub const KIND_HELLO: u8 = 1;
pub const KIND_INJECT: u8 = 2;
pub const KIND_PING: u8 = 3;
pub const KIND_WELCOME: u8 = 0x81;
pub const KIND_INJECTED: u8 = 0x82;
pub const KIND_REFUSED: u8 = 0x83;
pub const KIND_PONG: u8 = 0x84;

// Windows flag values (winuser.h), kept here so the protocol stays pure.
pub const MOUSEEVENTF_MOVE: u32 = 0x0001;
pub const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
pub const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
pub const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
pub const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
pub const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
pub const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
pub const MOUSEEVENTF_XDOWN: u32 = 0x0080;
pub const MOUSEEVENTF_XUP: u32 = 0x0100;
pub const MOUSEEVENTF_WHEEL: u32 = 0x0800;
pub const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
pub const MOUSEEVENTF_MOVE_NOCOALESCE: u32 = 0x2000;
pub const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
pub const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const MOUSE_ALLOWED: u32 = MOUSEEVENTF_MOVE
    | MOUSEEVENTF_LEFTDOWN
    | MOUSEEVENTF_LEFTUP
    | MOUSEEVENTF_RIGHTDOWN
    | MOUSEEVENTF_RIGHTUP
    | MOUSEEVENTF_MIDDLEDOWN
    | MOUSEEVENTF_MIDDLEUP
    | MOUSEEVENTF_XDOWN
    | MOUSEEVENTF_XUP
    | MOUSEEVENTF_WHEEL
    | MOUSEEVENTF_HWHEEL
    | MOUSEEVENTF_MOVE_NOCOALESCE
    | MOUSEEVENTF_VIRTUALDESK
    | MOUSEEVENTF_ABSOLUTE;
pub const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
pub const KEYEVENTF_KEYUP: u32 = 0x0002;
pub const KEYEVENTF_UNICODE: u32 = 0x0004;
pub const KEYEVENTF_SCANCODE: u32 = 0x0008;
const KEY_ALLOWED: u32 =
    KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP | KEYEVENTF_UNICODE | KEYEVENTF_SCANCODE;
/// Relative motion bound; the host sends i16 deltas.
const MAX_RELATIVE: i32 = 65_535;

/// One Windows `INPUT` record (without `time`/`dwExtraInfo`, always 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    Mouse {
        dx: i32,
        dy: i32,
        data: u32,
        flags: u32,
    },
    Key {
        vk: u16,
        scan: u16,
        flags: u32,
    },
}

impl Record {
    /// Bounds and flag checks applied by the agent to every record (and by
    /// the host before choosing the agent path).
    pub fn validate(&self) -> Result<(), &'static str> {
        match *self {
            Record::Mouse {
                dx,
                dy,
                data,
                flags,
            } => {
                if flags == 0 || flags & !MOUSE_ALLOWED != 0 {
                    return Err("unknown mouse flags");
                }
                let wheel = flags & (MOUSEEVENTF_WHEEL | MOUSEEVENTF_HWHEEL);
                let xbutton = flags & (MOUSEEVENTF_XDOWN | MOUSEEVENTF_XUP);
                if wheel == MOUSEEVENTF_WHEEL | MOUSEEVENTF_HWHEEL
                    || xbutton == MOUSEEVENTF_XDOWN | MOUSEEVENTF_XUP
                    || (wheel != 0 && xbutton != 0)
                {
                    return Err("conflicting mouse flags");
                }
                if wheel != 0 {
                    let delta = data as i32;
                    if !(i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&delta) {
                        return Err("wheel delta out of range");
                    }
                } else if xbutton != 0 {
                    if data != 1 && data != 2 {
                        return Err("invalid X button");
                    }
                } else if data != 0 {
                    return Err("unexpected mouse data");
                }
                let moves = flags & MOUSEEVENTF_MOVE != 0;
                if flags & MOUSEEVENTF_ABSOLUTE != 0 {
                    if !moves || !(0..=65_535).contains(&dx) || !(0..=65_535).contains(&dy) {
                        return Err("absolute position out of range");
                    }
                } else if flags & MOUSEEVENTF_VIRTUALDESK != 0 {
                    return Err("virtual desk without absolute");
                } else if moves {
                    if dx.unsigned_abs() > MAX_RELATIVE as u32
                        || dy.unsigned_abs() > MAX_RELATIVE as u32
                    {
                        return Err("relative motion out of range");
                    }
                } else if dx != 0 || dy != 0 {
                    return Err("motion without move flag");
                }
                Ok(())
            }
            Record::Key { vk, scan, flags } => {
                if flags & !KEY_ALLOWED != 0 {
                    return Err("unknown key flags");
                }
                if flags & KEYEVENTF_UNICODE != 0 {
                    if vk != 0 || flags & (KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY) != 0 {
                        return Err("invalid unicode key");
                    }
                } else if flags & KEYEVENTF_SCANCODE != 0 {
                    if scan == 0 || scan > 0xFF || vk > 0xFF {
                        return Err("invalid scan code");
                    }
                } else if vk == 0 || vk > 0xFE || scan > 0xFF {
                    return Err("invalid virtual key");
                }
                Ok(())
            }
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Record::Mouse {
                dx,
                dy,
                data,
                flags,
            } => {
                out.push(1);
                out.extend_from_slice(&dx.to_le_bytes());
                out.extend_from_slice(&dy.to_le_bytes());
                out.extend_from_slice(&data.to_le_bytes());
                out.extend_from_slice(&flags.to_le_bytes());
            }
            Record::Key { vk, scan, flags } => {
                out.push(2);
                out.extend_from_slice(&vk.to_le_bytes());
                out.extend_from_slice(&scan.to_le_bytes());
                out.extend_from_slice(&flags.to_le_bytes());
                out.extend_from_slice(&[0; 8]);
            }
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self, &'static str> {
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        let u16_at = |at: usize| u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
        let record = match bytes[0] {
            1 => Record::Mouse {
                dx: u32_at(1) as i32,
                dy: u32_at(5) as i32,
                data: u32_at(9),
                flags: u32_at(13),
            },
            2 => {
                if bytes[9..17].iter().any(|&b| b != 0) {
                    return Err("reserved key bytes set");
                }
                Record::Key {
                    vk: u16_at(1),
                    scan: u16_at(3),
                    flags: u32_at(5),
                }
            }
            _ => return Err("unknown record type"),
        };
        record.validate()?;
        Ok(record)
    }
}

/// Who opened the connection (logged by the agent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Host = 1,
    /// The tray checking availability and version; sends nothing else.
    Probe = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientMessage {
    Hello { min: u16, max: u16, role: Role },
    Inject(Vec<Record>),
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseCode {
    Version = 1,
    ExpectedHello = 2,
    Invalid = 3,
    RateLimited = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessage {
    Welcome {
        version: u16,
        agent_min: u16,
        agent_max: u16,
        agent_pid: u32,
        elevated: bool,
    },
    Injected {
        requested: u16,
        applied: u16,
        error: i32,
    },
    Refused {
        code: RefuseCode,
        agent_min: u16,
        agent_max: u16,
    },
    Pong,
}

/// Highest version both sides speak, if their ranges overlap.
pub fn negotiate(client_min: u16, client_max: u16, agent_min: u16, agent_max: u16) -> Option<u16> {
    if client_min > client_max || agent_min > agent_max {
        return None;
    }
    let version = client_max.min(agent_max);
    (version >= client_min.max(agent_min)).then_some(version)
}

fn invalid(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

impl ClientMessage {
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            ClientMessage::Hello { min, max, role } => {
                let mut payload = Vec::with_capacity(5);
                payload.extend_from_slice(&min.to_le_bytes());
                payload.extend_from_slice(&max.to_le_bytes());
                payload.push(*role as u8);
                (KIND_HELLO, payload)
            }
            ClientMessage::Inject(records) => {
                let mut payload = Vec::with_capacity(2 + records.len() * RECORD_LEN);
                payload.extend_from_slice(&(records.len() as u16).to_le_bytes());
                for record in records {
                    record.encode(&mut payload);
                }
                (KIND_INJECT, payload)
            }
            ClientMessage::Ping => (KIND_PING, Vec::new()),
        }
    }

    /// Strict decoding: exact lengths, known kinds, every record validated.
    pub fn decode(kind: u8, payload: &[u8]) -> io::Result<Self> {
        match kind {
            KIND_HELLO if payload.len() == 5 => {
                let role = match payload[4] {
                    1 => Role::Host,
                    2 => Role::Probe,
                    _ => return Err(invalid("unknown client role")),
                };
                Ok(ClientMessage::Hello {
                    min: u16::from_le_bytes([payload[0], payload[1]]),
                    max: u16::from_le_bytes([payload[2], payload[3]]),
                    role,
                })
            }
            KIND_INJECT if payload.len() >= 2 => {
                let count = u16::from_le_bytes([payload[0], payload[1]]) as usize;
                if count == 0 || count > MAX_RECORDS_PER_FRAME {
                    return Err(invalid("record count out of range"));
                }
                if payload.len() != 2 + count * RECORD_LEN {
                    return Err(invalid("inject length mismatch"));
                }
                payload[2..]
                    .chunks_exact(RECORD_LEN)
                    .map(|chunk| Record::decode(chunk).map_err(invalid))
                    .collect::<io::Result<Vec<_>>>()
                    .map(ClientMessage::Inject)
            }
            KIND_PING if payload.is_empty() => Ok(ClientMessage::Ping),
            _ => Err(invalid("unknown or malformed client message")),
        }
    }
}

impl AgentMessage {
    pub fn encode(&self) -> (u8, Vec<u8>) {
        match *self {
            AgentMessage::Welcome {
                version,
                agent_min,
                agent_max,
                agent_pid,
                elevated,
            } => {
                let mut payload = Vec::with_capacity(11);
                payload.extend_from_slice(&version.to_le_bytes());
                payload.extend_from_slice(&agent_min.to_le_bytes());
                payload.extend_from_slice(&agent_max.to_le_bytes());
                payload.extend_from_slice(&agent_pid.to_le_bytes());
                payload.push(u8::from(elevated));
                (KIND_WELCOME, payload)
            }
            AgentMessage::Injected {
                requested,
                applied,
                error,
            } => {
                let mut payload = Vec::with_capacity(8);
                payload.extend_from_slice(&requested.to_le_bytes());
                payload.extend_from_slice(&applied.to_le_bytes());
                payload.extend_from_slice(&error.to_le_bytes());
                (KIND_INJECTED, payload)
            }
            AgentMessage::Refused {
                code,
                agent_min,
                agent_max,
            } => {
                let mut payload = vec![code as u8];
                payload.extend_from_slice(&agent_min.to_le_bytes());
                payload.extend_from_slice(&agent_max.to_le_bytes());
                (KIND_REFUSED, payload)
            }
            AgentMessage::Pong => (KIND_PONG, Vec::new()),
        }
    }

    pub fn decode(kind: u8, payload: &[u8]) -> io::Result<Self> {
        let u16_at = |at: usize| u16::from_le_bytes([payload[at], payload[at + 1]]);
        match kind {
            KIND_WELCOME if payload.len() == 11 && payload[10] <= 1 => Ok(AgentMessage::Welcome {
                version: u16_at(0),
                agent_min: u16_at(2),
                agent_max: u16_at(4),
                agent_pid: u32::from_le_bytes(payload[6..10].try_into().unwrap()),
                elevated: payload[10] == 1,
            }),
            KIND_INJECTED if payload.len() == 8 => Ok(AgentMessage::Injected {
                requested: u16_at(0),
                applied: u16_at(2),
                error: i32::from_le_bytes(payload[4..8].try_into().unwrap()),
            }),
            KIND_REFUSED if payload.len() == 5 => {
                let code = match payload[0] {
                    1 => RefuseCode::Version,
                    2 => RefuseCode::ExpectedHello,
                    3 => RefuseCode::Invalid,
                    4 => RefuseCode::RateLimited,
                    _ => return Err(invalid("unknown refuse code")),
                };
                Ok(AgentMessage::Refused {
                    code,
                    agent_min: u16_at(1),
                    agent_max: u16_at(3),
                })
            }
            KIND_PONG if payload.is_empty() => Ok(AgentMessage::Pong),
            _ => Err(invalid("unknown or malformed agent message")),
        }
    }
}

pub fn write_frame<W: Write>(writer: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "admin agent payload exceeds limit",
        ));
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&MAGIC);
    frame.push(kind);
    frame.push(0);
    frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    frame.extend_from_slice(payload);
    // One write per frame keeps pipe messages whole.
    writer.write_all(&frame)?;
    writer.flush()
}

pub fn read_frame<R: Read>(reader: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[..4] != MAGIC {
        return Err(invalid("bad admin agent magic"));
    }
    if header[5] != 0 {
        return Err(invalid("reserved header byte set"));
    }
    let len = u16::from_le_bytes([header[6], header[7]]) as usize;
    if len > MAX_PAYLOAD {
        return Err(invalid("admin agent payload exceeds limit"));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok((header[4], payload))
}

pub fn write_client<W: Write>(writer: &mut W, message: &ClientMessage) -> io::Result<()> {
    let (kind, payload) = message.encode();
    write_frame(writer, kind, &payload)
}

pub fn write_agent<W: Write>(writer: &mut W, message: &AgentMessage) -> io::Result<()> {
    let (kind, payload) = message.encode();
    write_frame(writer, kind, &payload)
}

pub fn read_agent<R: Read>(reader: &mut R) -> io::Result<AgentMessage> {
    let (kind, payload) = read_frame(reader)?;
    AgentMessage::decode(kind, &payload)
}

/// Stable 64-bit FNV-1a, used to derive per-test-instance object names.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Kernel-object / pipe name for this instance. With a test data dir
/// (`BUBBLEPLAY_TRAY_DATA_DIR`) names get a suffix so a test tray/agent never
/// collides with the real one.
pub fn scoped_name(base: &str, test_data_dir: Option<&std::ffi::OsStr>) -> String {
    match test_data_dir {
        Some(dir) => {
            let key = dir.to_string_lossy().to_lowercase();
            format!("{base}.test-{:016x}", fnv1a64(key.as_bytes()))
        }
        None => base.to_string(),
    }
}

/// Pipe name of the admin agent for a terminal session.
pub fn pipe_name(session_id: u32, test_data_dir: Option<&std::ffi::OsStr>) -> String {
    scoped_name(
        &format!(r"\\.\pipe\BubblePlay.AdminAgent.s{session_id}"),
        test_data_dir,
    )
}

/// Pipe rights for the interactive user: read + write data (+ attributes),
/// but not FILE_CREATE_PIPE_INSTANCE (0x4), so no user process can add a
/// server instance to the agent's pipe.
pub const USER_PIPE_RIGHTS: u32 = 0x0012_018B;

/// Security descriptor (SDDL) of the agent's pipe. Elevated: owned by
/// Administrators, SYSTEM/Administrators full control, the user limited to
/// `USER_PIPE_RIGHTS`. The unelevated dev agent must create further
/// instances itself, so its user keeps full control (it holds no privilege).
/// The medium no-write-up label keeps low-integrity (sandboxed) processes of
/// the user out in both cases.
pub fn pipe_sddl(user_sid: &str, elevated: bool) -> String {
    if elevated {
        format!(
            "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;{USER_PIPE_RIGHTS:#010x};;;{user_sid})S:(ML;;NW;;;ME)"
        )
    } else {
        format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})S:(ML;;NW;;;ME)")
    }
}

/// Held by the running agent; the tray watches it for liveness.
pub const AGENT_MUTEX: &str = r"Local\BubblePlay.AdminAgent";
/// Created by the tray; signaled to stop the agent. Its absence at agent
/// start means the tray is gone.
pub const AGENT_STOP_EVENT: &str = r"Local\BubblePlayTray.AgentStop";
/// Held by the running tray.
pub const TRAY_MUTEX: &str = r"Local\BubblePlayTray.SingleInstance";
/// Environment variables the tray hands to the host.
pub const ENV_PIPE: &str = "BUBBLEPLAY_ADMIN_AGENT_PIPE";
/// Test mode only: accept an agent that is not running from Program Files.
pub const ENV_ALLOW_DEV_AGENT: &str = "BUBBLEPLAY_ADMIN_AGENT_ALLOW_DEV";

#[cfg(test)]
mod tests {
    use super::*;

    fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> Record {
        Record::Mouse {
            dx,
            dy,
            data,
            flags,
        }
    }

    fn key(vk: u16, scan: u16, flags: u32) -> Record {
        Record::Key { vk, scan, flags }
    }

    #[test]
    fn every_message_round_trips() {
        let clients = [
            ClientMessage::Hello {
                min: 1,
                max: 3,
                role: Role::Host,
            },
            ClientMessage::Hello {
                min: 1,
                max: 1,
                role: Role::Probe,
            },
            ClientMessage::Inject(vec![
                mouse(-5, 7, 0, MOUSEEVENTF_MOVE),
                mouse(
                    65_535,
                    0,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ),
                mouse(0, 0, (-120i32) as u32, MOUSEEVENTF_WHEEL),
                mouse(0, 0, 2, MOUSEEVENTF_XDOWN),
                key(0, 0x1D, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY),
                key(0x13, 0, KEYEVENTF_KEYUP),
                key(0, 0x20AC, KEYEVENTF_UNICODE),
            ]),
            ClientMessage::Ping,
        ];
        for message in clients {
            let mut wire = Vec::new();
            write_client(&mut wire, &message).unwrap();
            let (kind, payload) = read_frame(&mut wire.as_slice()).unwrap();
            assert_eq!(ClientMessage::decode(kind, &payload).unwrap(), message);
        }
        let agents = [
            AgentMessage::Welcome {
                version: 1,
                agent_min: 1,
                agent_max: 2,
                agent_pid: 77,
                elevated: true,
            },
            AgentMessage::Injected {
                requested: 3,
                applied: 2,
                error: 5,
            },
            AgentMessage::Refused {
                code: RefuseCode::Version,
                agent_min: 2,
                agent_max: 4,
            },
            AgentMessage::Pong,
        ];
        for message in agents {
            let mut wire = Vec::new();
            write_agent(&mut wire, &message).unwrap();
            assert_eq!(read_agent(&mut wire.as_slice()).unwrap(), message);
        }
    }

    #[test]
    fn frozen_hello_layout() {
        let mut wire = Vec::new();
        write_client(
            &mut wire,
            &ClientMessage::Hello {
                min: 1,
                max: 2,
                role: Role::Host,
            },
        )
        .unwrap();
        assert_eq!(wire, b"BPAG\x01\x00\x05\x00\x01\x00\x02\x00\x01");
    }

    #[test]
    fn frames_are_bounded_and_strict() {
        // Bad magic, reserved byte, oversized length.
        assert!(read_frame(&mut &b"XPAG\x01\x00\x00\x00"[..]).is_err());
        assert!(read_frame(&mut &b"BPAG\x01\x01\x00\x00"[..]).is_err());
        let mut oversized = b"BPAG\x02\x00".to_vec();
        oversized.extend_from_slice(&((MAX_PAYLOAD + 1) as u16).to_le_bytes());
        assert!(read_frame(&mut oversized.as_slice()).is_err());
        // Truncated payload.
        assert!(read_frame(&mut &b"BPAG\x03\x00\x02\x00\x00"[..]).is_err());
        assert!(write_frame(&mut Vec::new(), KIND_INJECT, &vec![0; MAX_PAYLOAD + 1]).is_err());
    }

    #[test]
    fn client_messages_are_validated() {
        // Unknown kind, wrong lengths, unknown role.
        assert!(ClientMessage::decode(9, &[]).is_err());
        assert!(ClientMessage::decode(KIND_HELLO, &[1, 0, 1, 0]).is_err());
        assert!(ClientMessage::decode(KIND_HELLO, &[1, 0, 1, 0, 3]).is_err());
        assert!(ClientMessage::decode(KIND_PING, &[0]).is_err());
        // Record count zero, too many, and length mismatch.
        assert!(ClientMessage::decode(KIND_INJECT, &[0, 0]).is_err());
        let too_many = ((MAX_RECORDS_PER_FRAME + 1) as u16).to_le_bytes();
        assert!(ClientMessage::decode(KIND_INJECT, &too_many).is_err());
        assert!(ClientMessage::decode(KIND_INJECT, &[1, 0, 1, 0]).is_err());
        // Unknown record type.
        let mut payload = vec![1, 0, 9];
        payload.extend_from_slice(&[0; 16]);
        assert!(ClientMessage::decode(KIND_INJECT, &payload).is_err());
        // Reserved key bytes must be zero.
        let (_, mut payload) = ClientMessage::Inject(vec![key(0x41, 0, 0)]).encode();
        *payload.last_mut().unwrap() = 1;
        assert!(ClientMessage::decode(KIND_INJECT, &payload).is_err());
        // One bad record rejects the whole frame.
        let (_, payload) = ClientMessage::Inject(vec![
            mouse(1, 1, 0, MOUSEEVENTF_MOVE),
            mouse(0, 0, 0, 0x10_0000),
        ])
        .encode();
        assert!(ClientMessage::decode(KIND_INJECT, &payload).is_err());
    }

    #[test]
    fn record_bounds() {
        let bad = [
            mouse(0, 0, 0, 0),
            mouse(0, 0, 0, 0x10_0000),
            mouse(0, 0, 0, MOUSEEVENTF_WHEEL | MOUSEEVENTF_HWHEEL),
            mouse(0, 0, 1, MOUSEEVENTF_XDOWN | MOUSEEVENTF_XUP),
            mouse(0, 0, 1, MOUSEEVENTF_XDOWN | MOUSEEVENTF_WHEEL),
            mouse(0, 0, 3, MOUSEEVENTF_XDOWN),
            mouse(0, 0, 40_000, MOUSEEVENTF_WHEEL),
            mouse(0, 0, 5, MOUSEEVENTF_LEFTDOWN),
            mouse(70_000, 0, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE),
            mouse(-1, 0, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE),
            mouse(1, 1, 0, MOUSEEVENTF_ABSOLUTE),
            mouse(1, 1, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_VIRTUALDESK),
            mouse(70_000, 0, 0, MOUSEEVENTF_MOVE),
            mouse(3, 0, 0, MOUSEEVENTF_LEFTDOWN),
            key(0, 0, 0),
            key(0x100, 0, 0),
            key(0xFF, 0, 0),
            key(0x41, 0, 0x10),
            key(0, 0, KEYEVENTF_SCANCODE),
            key(0, 0x100, KEYEVENTF_SCANCODE),
            key(0x41, 0x41, KEYEVENTF_UNICODE),
            key(0, 0x41, KEYEVENTF_UNICODE | KEYEVENTF_SCANCODE),
        ];
        for record in bad {
            assert!(record.validate().is_err(), "{record:?}");
        }
        let good = [
            mouse(-32_768, 32_767, 0, MOUSEEVENTF_MOVE),
            mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            mouse(0, 0, 0, MOUSEEVENTF_RIGHTUP | MOUSEEVENTF_MIDDLEUP),
            mouse(0, 0, (-32_768i32) as u32, MOUSEEVENTF_HWHEEL),
            mouse(0, 0, 1, MOUSEEVENTF_XUP),
            key(0x41, 0, 0),
            key(0x5B, 0, KEYEVENTF_KEYUP | KEYEVENTF_EXTENDEDKEY),
            key(0, 0xFF, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP),
            key(0, 0xD83D, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
        ];
        for record in good {
            assert!(record.validate().is_ok(), "{record:?}");
        }
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(negotiate(1, 1, 1, 1), Some(1));
        assert_eq!(negotiate(1, 3, 1, 2), Some(2));
        assert_eq!(negotiate(2, 3, 1, 5), Some(3));
        assert_eq!(negotiate(1, 1, 2, 3), None);
        assert_eq!(negotiate(4, 5, 1, 3), None);
        assert_eq!(negotiate(3, 1, 1, 3), None);
        assert_eq!(negotiate(1, 3, 3, 2), None);
    }

    #[test]
    fn elevated_pipe_denies_users_new_instances() {
        const FILE_CREATE_PIPE_INSTANCE: u32 = 0x4;
        assert_eq!(USER_PIPE_RIGHTS & FILE_CREATE_PIPE_INSTANCE, 0);
        assert_eq!(USER_PIPE_RIGHTS & 0x1_0000, 0); // DELETE
        assert_eq!(USER_PIPE_RIGHTS & 0xC_0000, 0); // WRITE_DAC | WRITE_OWNER
        assert_eq!(USER_PIPE_RIGHTS & 0x3, 0x3); // read + write data
        let sddl = pipe_sddl("S-1-5-21-1-2-3-1001", true);
        assert_eq!(
            sddl,
            "O:BAD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x0012018b;;;S-1-5-21-1-2-3-1001)S:(ML;;NW;;;ME)"
        );
        assert!(!sddl.contains("GA;;;S-1-5-21"));
        assert!(pipe_sddl("S-1-5-21-1-2-3-1001", false).contains("(A;;GA;;;S-1-5-21-1-2-3-1001)"));
    }

    #[test]
    fn names_are_scoped_per_test_data_dir() {
        assert_eq!(pipe_name(2, None), r"\\.\pipe\BubblePlay.AdminAgent.s2");
        let a = pipe_name(2, Some(std::ffi::OsStr::new(r"C:\Temp\A")));
        let a_upper = pipe_name(2, Some(std::ffi::OsStr::new(r"C:\TEMP\a")));
        let b = pipe_name(2, Some(std::ffi::OsStr::new(r"C:\Temp\B")));
        assert_eq!(a, a_upper);
        assert_ne!(a, b);
        assert!(a.starts_with(r"\\.\pipe\BubblePlay.AdminAgent.s2.test-"));
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
