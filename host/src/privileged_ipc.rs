//! Versioned binary protocol used across BubblePlay's privileged local boundary.
//!
//! The frame header is always 20 bytes, little-endian:
//! `magic[4], version:u16, kind:u16, flags:u32, payload_len:u32, sequence:u32`.
//! Control messages are capped at 64 KiB and encoded access-unit messages at
//! 8 MiB. The decoder validates the header before allocating the payload.

use std::fmt;
use std::io::{self, Read, Write};

pub const MAGIC: [u8; 4] = *b"BPIP";
// Native cursor-kind events were added in v5. Host and privileged Agent must
// be built/upgraded together; mismatched versions fail before payload parsing.
pub const PROTOCOL_VERSION: u16 = 5;
pub const HEADER_LEN: usize = 20;
pub const MAX_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_MEDIA_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
const MAX_MONITORS: usize = 32;
const MAX_MONITOR_NAME_BYTES: usize = 256;

#[derive(Debug)]
pub enum ProtocolError {
    Io(io::Error),
    TruncatedHeader,
    TruncatedPayload {
        expected: usize,
        received: usize,
    },
    InvalidMagic([u8; 4]),
    UnsupportedVersion(u16),
    UnknownMessageKind(u16),
    PayloadTooLarge {
        kind: MessageKind,
        size: usize,
        max: usize,
    },
    MalformedPayload {
        kind: MessageKind,
        reason: &'static str,
    },
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "IPC I/O error: {error}"),
            Self::TruncatedHeader => f.write_str("truncated IPC frame header"),
            Self::TruncatedPayload { expected, received } => write!(
                f,
                "truncated IPC frame payload: expected {expected} bytes, received {received}"
            ),
            Self::InvalidMagic(magic) => write!(f, "invalid IPC magic {magic:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported IPC protocol version {version}")
            }
            Self::UnknownMessageKind(kind) => write!(f, "unknown IPC message kind {kind}"),
            Self::PayloadTooLarge { kind, size, max } => {
                write!(f, "IPC {kind:?} payload is {size} bytes; maximum is {max}")
            }
            Self::MalformedPayload { kind, reason } => {
                write!(f, "malformed IPC {kind:?} payload: {reason}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    RenewLease = 1,
    EncodePermit = 2,
    ConfigureMedia = 3,
    SetBitrate = 4,
    ForceIdr = 5,
    Input = 6,
    SelectMonitor = 7,
    ReleaseAll = 8,
    Shutdown = 9,
    ListMonitors = 10,
    RequestIdleSnapshot = 11,
    Capabilities = 0x100,
    DesktopTransition = 0x101,
    CaptureHealth = 0x102,
    MonitorList = 0x103,
    Cursor = 0x104,
    ConfigAck = 0x105,
    NoFrame = 0x106,
    EncodedAccessUnit = 0x107,
    MonitorSwitchResult = 0x108,
    InputStats = 0x109,
    RecoverableError = 0x10a,
    FatalError = 0x10b,
    ClipboardText = 0x10c,
    ClipboardPng = 0x10e,
    CursorKind = 0x10d,
}

impl MessageKind {
    fn from_u16(value: u16) -> Result<Self, ProtocolError> {
        Ok(match value {
            1 => Self::RenewLease,
            2 => Self::EncodePermit,
            3 => Self::ConfigureMedia,
            4 => Self::SetBitrate,
            5 => Self::ForceIdr,
            6 => Self::Input,
            7 => Self::SelectMonitor,
            8 => Self::ReleaseAll,
            9 => Self::Shutdown,
            10 => Self::ListMonitors,
            11 => Self::RequestIdleSnapshot,
            0x100 => Self::Capabilities,
            0x101 => Self::DesktopTransition,
            0x102 => Self::CaptureHealth,
            0x103 => Self::MonitorList,
            0x104 => Self::Cursor,
            0x105 => Self::ConfigAck,
            0x106 => Self::NoFrame,
            0x107 => Self::EncodedAccessUnit,
            0x108 => Self::MonitorSwitchResult,
            0x109 => Self::InputStats,
            0x10a => Self::RecoverableError,
            0x10b => Self::FatalError,
            0x10c => Self::ClipboardText,
            0x10e => Self::ClipboardPng,
            0x10d => Self::CursorKind,
            _ => return Err(ProtocolError::UnknownMessageKind(value)),
        })
    }

    fn payload_limit(self) -> usize {
        if self == Self::EncodedAccessUnit {
            MAX_MEDIA_PAYLOAD_BYTES
        } else {
            MAX_CONTROL_PAYLOAD_BYTES
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264 = 1,
    Hevc = 2,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Work = 1,
    Gaming = 2,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderPreset {
    P1 = 1,
    P2 = 2,
    P3 = 3,
    P4 = 4,
    P5 = 5,
    P6 = 6,
    P7 = 7,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderTuning {
    UltraLowLatency = 1,
    LowLatency = 2,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderMultipass {
    Disabled = 0,
    Quarter = 1,
    Full = 2,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderAqMode {
    None = 0,
    Spatial = 1,
    Temporal = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropRect {
    Q16 {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    Pixels {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    },
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Unknown = 0,
    Default = 1,
    Winlogon = 2,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopTransitionReason {
    Startup = 1,
    DesktopSwitched = 2,
    SessionChanged = 3,
    AgentRestarted = 4,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    Active = 1,
    Unavailable = 2,
    Recovering = 3,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoFrameReason {
    Timeout = 1,
    DesktopUnavailable = 2,
    Backpressured = 3,
    NoChange = 4,
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOperation {
    Session = 1,
    Desktop = 2,
    Capture = 3,
    Scale = 4,
    Encode = 5,
    Input = 6,
    Monitor = 7,
    Ipc = 8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermitIdentity {
    pub permit_id: u64,
    pub session_generation: u32,
    pub config_revision: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaConfig {
    pub session_generation: u32,
    pub config_revision: u32,
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_bps: u32,
    pub stream_mode: StreamMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub flags: u32,
    pub max_width: u16,
    pub max_height: u16,
    pub max_fps: u16,
    pub max_bitrate_bps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    pub primary: bool,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorState {
    pub acknowledged_sequence: Option<u16>,
    pub x: u16,
    pub y: u16,
    pub in_bounds: bool,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedAccessUnit {
    pub permit_id: u64,
    pub session_generation: u32,
    pub config_revision: u32,
    pub timestamp_90khz: u64,
    pub keyframe: bool,
    pub codec: Codec,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub encode_us: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorSwitchResult {
    pub session_generation: u32,
    pub monitor_id: u32,
    pub accepted: bool,
    pub width: u16,
    pub height: u16,
    pub error_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputStatistics {
    pub session_generation: u32,
    /// Valid packets received by the input worker. This is never application proof.
    pub received_packets: u64,
    /// Packets whose complete non-empty SendInput plan Windows accepted.
    pub accepted_packets: u64,
    /// Packets for which at least one requested SendInput event was not applied.
    pub failed_packets: u64,
    pub dropped_packets: u64,
    pub released_inputs: u64,
    pub requested_events: u64,
    pub applied_events: u64,
    pub last_error_code: Option<i32>,
    pub last_sequence: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentError {
    pub operation: AgentOperation,
    pub error_code: i32,
    pub session_generation: u32,
    pub config_revision: u32,
    pub permit_id: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    RenewLease {
        lease_id: u64,
        ttl_ms: u32,
    },
    EncodePermit {
        permit_id: u64,
        session_generation: u32,
        config_revision: u32,
        stream_mode: StreamMode,
        width: u16,
        height: u16,
        fps: u16,
        bitrate_bps: u32,
        codec: Codec,
        preset: EncoderPreset,
        tuning: EncoderTuning,
        multipass: EncoderMultipass,
        aq_mode: EncoderAqMode,
        aq_strength: u8,
        vbv_frames: u8,
        lookahead_depth: u8,
        weighted_prediction: bool,
        gop_seconds: u8,
        crop: Option<CropRect>,
        force_idr: bool,
        allow_idle_snapshot: bool,
    },
    ConfigureMedia(MediaConfig),
    SetBitrate {
        bitrate_bps: u32,
    },
    ForceIdr {
        generation: u32,
    },
    Input(Vec<u8>),
    SelectMonitor {
        monitor_id: u32,
    },
    ReleaseAll,
    Shutdown,
    ListMonitors,
    RequestIdleSnapshot {
        permit_id: u64,
        session_generation: u32,
        config_revision: u32,
    },
    Capabilities(Capabilities),
    DesktopTransition {
        desktop: Desktop,
        generation: u32,
        input_enabled: bool,
        reason: DesktopTransitionReason,
    },
    CaptureHealth {
        state: CaptureState,
        error_code: i32,
    },
    MonitorList(Vec<MonitorInfo>),
    Cursor(CursorState),
    CursorKind(crate::input::CursorKind),
    ConfigAck {
        session_generation: u32,
        config_revision: u32,
        accepted: bool,
        error_code: i32,
    },
    NoFrame {
        permit_id: u64,
        session_generation: u32,
        config_revision: u32,
        reason: NoFrameReason,
    },
    EncodedAccessUnit(EncodedAccessUnit),
    MonitorSwitchResult(MonitorSwitchResult),
    InputStats(InputStatistics),
    RecoverableError(AgentError),
    FatalError(AgentError),
    /// One in-order chunk of interactive-session clipboard text (UTF-8,
    /// split on character boundaries, at most `MAX_CLIPBOARD_CHUNK_BYTES`).
    ClipboardText {
        id: u32,
        index: u16,
        count: u16,
        text: String,
    },
    /// Base64 PNG chunks, bounded to an 8 MiB image.
    ClipboardPng { id: u32, index: u16, count: u16, text: String },
}

/// Clipboard chunks stay well inside the control payload limit.
pub const MAX_CLIPBOARD_CHUNK_BYTES: usize = crate::clipboard::HOST_CHUNK_BYTES;
const MAX_CLIPBOARD_CHUNKS: u16 = 64;
const MAX_PNG_CHUNKS: u16 = crate::clipboard_image::MAX_PNG_WIRE_BYTES.div_ceil(MAX_CLIPBOARD_CHUNK_BYTES) as u16;

impl Message {
    pub fn kind(&self) -> MessageKind {
        match self {
            Self::RenewLease { .. } => MessageKind::RenewLease,
            Self::EncodePermit { .. } => MessageKind::EncodePermit,
            Self::ConfigureMedia(_) => MessageKind::ConfigureMedia,
            Self::SetBitrate { .. } => MessageKind::SetBitrate,
            Self::ForceIdr { .. } => MessageKind::ForceIdr,
            Self::Input(_) => MessageKind::Input,
            Self::SelectMonitor { .. } => MessageKind::SelectMonitor,
            Self::ReleaseAll => MessageKind::ReleaseAll,
            Self::Shutdown => MessageKind::Shutdown,
            Self::ListMonitors => MessageKind::ListMonitors,
            Self::RequestIdleSnapshot { .. } => MessageKind::RequestIdleSnapshot,
            Self::Capabilities(_) => MessageKind::Capabilities,
            Self::DesktopTransition { .. } => MessageKind::DesktopTransition,
            Self::CaptureHealth { .. } => MessageKind::CaptureHealth,
            Self::MonitorList(_) => MessageKind::MonitorList,
            Self::Cursor(_) => MessageKind::Cursor,
            Self::CursorKind(_) => MessageKind::CursorKind,
            Self::ConfigAck { .. } => MessageKind::ConfigAck,
            Self::NoFrame { .. } => MessageKind::NoFrame,
            Self::EncodedAccessUnit(_) => MessageKind::EncodedAccessUnit,
            Self::MonitorSwitchResult(_) => MessageKind::MonitorSwitchResult,
            Self::InputStats(_) => MessageKind::InputStats,
            Self::RecoverableError(_) => MessageKind::RecoverableError,
            Self::FatalError(_) => MessageKind::FatalError,
            Self::ClipboardText { .. } => MessageKind::ClipboardText,
            Self::ClipboardPng { .. } => MessageKind::ClipboardPng,
        }
    }

    pub fn permit_identity(&self) -> Option<PermitIdentity> {
        match self {
            Self::EncodePermit {
                permit_id,
                session_generation,
                config_revision,
                ..
            }
            | Self::RequestIdleSnapshot {
                permit_id,
                session_generation,
                config_revision,
            }
            | Self::NoFrame {
                permit_id,
                session_generation,
                config_revision,
                ..
            } => Some(PermitIdentity {
                permit_id: *permit_id,
                session_generation: *session_generation,
                config_revision: *config_revision,
            }),
            Self::EncodedAccessUnit(access_unit) => Some(PermitIdentity {
                permit_id: access_unit.permit_id,
                session_generation: access_unit.session_generation,
                config_revision: access_unit.config_revision,
            }),
            Self::RecoverableError(error) | Self::FatalError(error) => {
                error.permit_id.map(|permit_id| PermitIdentity {
                    permit_id,
                    session_generation: error.session_generation,
                    config_revision: error.config_revision,
                })
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub flags: u32,
    pub sequence: u32,
    pub message: Message,
}

pub fn write_frame<W: Write>(
    writer: &mut W,
    flags: u32,
    sequence: u32,
    message: &Message,
) -> Result<(), ProtocolError> {
    let kind = message.kind();
    let payload = encode_message(message)?;
    enforce_payload_limit(kind, payload.len())?;

    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    header[8..12].copy_from_slice(&flags.to_le_bytes());
    header[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    header[16..20].copy_from_slice(&sequence.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&payload)?;
    Ok(())
}

/// Reads one complete frame. Clean EOF before a header returns `Ok(None)`;
/// EOF after any part of a frame is a protocol error.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>, ProtocolError> {
    let mut header = [0u8; HEADER_LEN];
    match read_first_byte(reader, &mut header[0])? {
        false => return Ok(None),
        true => {}
    }
    if read_count(reader, &mut header[1..])? != HEADER_LEN - 1 {
        return Err(ProtocolError::TruncatedHeader);
    }

    let magic = [header[0], header[1], header[2], header[3]];
    if magic != MAGIC {
        return Err(ProtocolError::InvalidMagic(magic));
    }
    let version = u16::from_le_bytes([header[4], header[5]]);
    if version != PROTOCOL_VERSION {
        return Err(ProtocolError::UnsupportedVersion(version));
    }
    let kind = MessageKind::from_u16(u16::from_le_bytes([header[6], header[7]]))?;
    let flags = u32::from_le_bytes(header[8..12].try_into().expect("fixed header slice"));
    let payload_len =
        u32::from_le_bytes(header[12..16].try_into().expect("fixed header slice")) as usize;
    let sequence = u32::from_le_bytes(header[16..20].try_into().expect("fixed header slice"));
    enforce_payload_limit(kind, payload_len)?;

    let mut payload = vec![0u8; payload_len];
    let received = read_count(reader, &mut payload)?;
    if received != payload_len {
        return Err(ProtocolError::TruncatedPayload {
            expected: payload_len,
            received,
        });
    }
    let message = decode_message(kind, &payload)?;
    Ok(Some(Frame {
        flags,
        sequence,
        message,
    }))
}

fn read_first_byte<R: Read>(reader: &mut R, byte: &mut u8) -> io::Result<bool> {
    loop {
        match reader.read(std::slice::from_mut(byte)) {
            Ok(0) => return Ok(false),
            Ok(1) => return Ok(true),
            Ok(_) => unreachable!("one-byte buffer"),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn read_count<R: Read>(reader: &mut R, mut buffer: &mut [u8]) -> io::Result<usize> {
    let wanted = buffer.len();
    while !buffer.is_empty() {
        match reader.read(buffer) {
            Ok(0) => break,
            Ok(read) => buffer = &mut buffer[read..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(wanted - buffer.len())
}

fn enforce_payload_limit(kind: MessageKind, size: usize) -> Result<(), ProtocolError> {
    let max = kind.payload_limit();
    if size > max {
        Err(ProtocolError::PayloadTooLarge { kind, size, max })
    } else {
        Ok(())
    }
}

fn encode_message(message: &Message) -> Result<Vec<u8>, ProtocolError> {
    let kind = message.kind();
    let mut out = Encoder::default();
    match message {
        Message::RenewLease { lease_id, ttl_ms } => {
            out.u64(*lease_id);
            out.u32(*ttl_ms);
        }
        Message::EncodePermit {
            permit_id,
            session_generation,
            config_revision,
            stream_mode,
            width,
            height,
            fps,
            bitrate_bps,
            codec,
            preset,
            tuning,
            multipass,
            aq_mode,
            aq_strength,
            vbv_frames,
            lookahead_depth,
            weighted_prediction,
            gop_seconds,
            crop,
            force_idr,
            allow_idle_snapshot,
        } => {
            out.u64(*permit_id);
            out.u32(*session_generation);
            out.u32(*config_revision);
            out.u8(*stream_mode as u8);
            out.u16(*width);
            out.u16(*height);
            out.u16(*fps);
            out.u32(*bitrate_bps);
            out.u8(*codec as u8);
            out.u8(*preset as u8);
            out.u8(*tuning as u8);
            out.u8(*multipass as u8);
            out.u8(*aq_mode as u8);
            out.u8(*aq_strength);
            out.u8(*vbv_frames);
            out.u8(*lookahead_depth);
            out.bool(*weighted_prediction);
            out.u8(*gop_seconds);
            encode_crop(&mut out, *crop);
            out.bool(*force_idr);
            out.bool(*allow_idle_snapshot);
        }
        Message::ConfigureMedia(config) => {
            out.u32(config.session_generation);
            out.u32(config.config_revision);
            out.u8(config.codec as u8);
            out.u16(config.width);
            out.u16(config.height);
            out.u16(config.fps);
            out.u32(config.bitrate_bps);
            out.u8(config.stream_mode as u8);
        }
        Message::SetBitrate { bitrate_bps } => out.u32(*bitrate_bps),
        Message::ForceIdr { generation } => out.u32(*generation),
        Message::Input(data) => out.bytes(data),
        Message::SelectMonitor { monitor_id } => out.u32(*monitor_id),
        Message::ReleaseAll | Message::Shutdown | Message::ListMonitors => {}
        Message::RequestIdleSnapshot {
            permit_id,
            session_generation,
            config_revision,
        } => {
            out.u64(*permit_id);
            out.u32(*session_generation);
            out.u32(*config_revision);
        }
        Message::Capabilities(capabilities) => {
            out.u32(capabilities.flags);
            out.u16(capabilities.max_width);
            out.u16(capabilities.max_height);
            out.u16(capabilities.max_fps);
            out.u32(capabilities.max_bitrate_bps);
        }
        Message::DesktopTransition {
            desktop,
            generation,
            input_enabled,
            reason,
        } => {
            out.u8(*desktop as u8);
            out.u32(*generation);
            out.bool(*input_enabled);
            out.u16(*reason as u16);
        }
        Message::CaptureHealth { state, error_code } => {
            out.u8(*state as u8);
            out.i32(*error_code);
        }
        Message::MonitorList(monitors) => {
            if monitors.len() > MAX_MONITORS {
                return malformed(kind, "too many monitors");
            }
            out.u16(monitors.len() as u16);
            for monitor in monitors {
                if monitor.name.len() > MAX_MONITOR_NAME_BYTES {
                    return malformed(kind, "monitor name is too long");
                }
                out.u32(monitor.id);
                out.u16(monitor.width);
                out.u16(monitor.height);
                out.bool(monitor.primary);
                out.u16(monitor.name.len() as u16);
                out.bytes(monitor.name.as_bytes());
            }
        }
        Message::CursorKind(kind) => out.u8(*kind as u8),
        Message::Cursor(cursor) => {
            out.u16(cursor.acknowledged_sequence.unwrap_or(0));
            out.bool(cursor.acknowledged_sequence.is_some());
            out.u16(cursor.x);
            out.u16(cursor.y);
            out.bool(cursor.in_bounds);
            out.bool(cursor.visible);
        }
        Message::ConfigAck {
            session_generation,
            config_revision,
            accepted,
            error_code,
        } => {
            out.u32(*session_generation);
            out.u32(*config_revision);
            out.bool(*accepted);
            out.i32(*error_code);
        }
        Message::NoFrame {
            permit_id,
            session_generation,
            config_revision,
            reason,
        } => {
            out.u64(*permit_id);
            out.u32(*session_generation);
            out.u32(*config_revision);
            out.u16(*reason as u16);
        }
        Message::EncodedAccessUnit(access_unit) => {
            out.u64(access_unit.permit_id);
            out.u32(access_unit.session_generation);
            out.u32(access_unit.config_revision);
            out.u64(access_unit.timestamp_90khz);
            out.bool(access_unit.keyframe);
            out.u8(access_unit.codec as u8);
            out.u16(access_unit.width);
            out.u16(access_unit.height);
            out.u16(access_unit.fps);
            out.u32(access_unit.encode_us);
            out.bytes(&access_unit.data);
        }
        Message::MonitorSwitchResult(result) => {
            out.u32(result.session_generation);
            out.u32(result.monitor_id);
            out.bool(result.accepted);
            out.u16(result.width);
            out.u16(result.height);
            out.i32(result.error_code);
        }
        Message::InputStats(stats) => {
            out.u32(stats.session_generation);
            out.u64(stats.received_packets);
            out.u64(stats.accepted_packets);
            out.u64(stats.failed_packets);
            out.u64(stats.dropped_packets);
            out.u64(stats.released_inputs);
            out.u64(stats.requested_events);
            out.u64(stats.applied_events);
            out.i32(stats.last_error_code.unwrap_or(0));
            out.bool(stats.last_error_code.is_some());
            out.u16(stats.last_sequence.unwrap_or(0));
            out.bool(stats.last_sequence.is_some());
        }
        Message::RecoverableError(error) | Message::FatalError(error) => {
            encode_agent_error(&mut out, error);
        }
        Message::ClipboardText { id, index, count, text }
        | Message::ClipboardPng { id, index, count, text } => {
            if text.len() > MAX_CLIPBOARD_CHUNK_BYTES
                || *count == 0
                || *count > if kind == MessageKind::ClipboardPng { MAX_PNG_CHUNKS } else { MAX_CLIPBOARD_CHUNKS }
                || index >= count
            {
                return malformed(kind, "invalid clipboard chunk");
            }
            out.u32(*id);
            out.u16(*index);
            out.u16(*count);
            out.bytes(text.as_bytes());
        }
    }
    Ok(out.0)
}

fn decode_message(kind: MessageKind, payload: &[u8]) -> Result<Message, ProtocolError> {
    let mut input = Decoder::new(kind, payload);
    let message = match kind {
        MessageKind::RenewLease => Message::RenewLease {
            lease_id: input.u64()?,
            ttl_ms: input.u32()?,
        },
        MessageKind::EncodePermit => Message::EncodePermit {
            permit_id: input.u64()?,
            session_generation: input.u32()?,
            config_revision: input.u32()?,
            stream_mode: decode_stream_mode(kind, input.u8()?)?,
            width: input.u16()?,
            height: input.u16()?,
            fps: input.u16()?,
            bitrate_bps: input.u32()?,
            codec: decode_codec(kind, input.u8()?)?,
            preset: decode_encoder_preset(kind, input.u8()?)?,
            tuning: decode_encoder_tuning(kind, input.u8()?)?,
            multipass: decode_encoder_multipass(kind, input.u8()?)?,
            aq_mode: decode_encoder_aq_mode(kind, input.u8()?)?,
            aq_strength: input.u8()?,
            vbv_frames: input.u8()?,
            lookahead_depth: input.u8()?,
            weighted_prediction: input.bool()?,
            gop_seconds: input.u8()?,
            crop: decode_crop(&mut input)?,
            force_idr: input.bool()?,
            allow_idle_snapshot: input.bool()?,
        },
        MessageKind::ConfigureMedia => Message::ConfigureMedia(MediaConfig {
            session_generation: input.u32()?,
            config_revision: input.u32()?,
            codec: decode_codec(kind, input.u8()?)?,
            width: input.u16()?,
            height: input.u16()?,
            fps: input.u16()?,
            bitrate_bps: input.u32()?,
            stream_mode: decode_stream_mode(kind, input.u8()?)?,
        }),
        MessageKind::SetBitrate => Message::SetBitrate {
            bitrate_bps: input.u32()?,
        },
        MessageKind::ForceIdr => Message::ForceIdr {
            generation: input.u32()?,
        },
        MessageKind::Input => Message::Input(input.rest().to_vec()),
        MessageKind::SelectMonitor => Message::SelectMonitor {
            monitor_id: input.u32()?,
        },
        MessageKind::ReleaseAll => Message::ReleaseAll,
        MessageKind::Shutdown => Message::Shutdown,
        MessageKind::ListMonitors => Message::ListMonitors,
        MessageKind::RequestIdleSnapshot => Message::RequestIdleSnapshot {
            permit_id: input.u64()?,
            session_generation: input.u32()?,
            config_revision: input.u32()?,
        },
        MessageKind::Capabilities => Message::Capabilities(Capabilities {
            flags: input.u32()?,
            max_width: input.u16()?,
            max_height: input.u16()?,
            max_fps: input.u16()?,
            max_bitrate_bps: input.u32()?,
        }),
        MessageKind::DesktopTransition => Message::DesktopTransition {
            desktop: decode_desktop(kind, input.u8()?)?,
            generation: input.u32()?,
            input_enabled: input.bool()?,
            reason: decode_desktop_reason(kind, input.u16()?)?,
        },
        MessageKind::CaptureHealth => Message::CaptureHealth {
            state: decode_capture_state(kind, input.u8()?)?,
            error_code: input.i32()?,
        },
        MessageKind::MonitorList => {
            let count = input.u16()? as usize;
            if count > MAX_MONITORS {
                return malformed(kind, "too many monitors");
            }
            let mut monitors = Vec::with_capacity(count);
            for _ in 0..count {
                let id = input.u32()?;
                let width = input.u16()?;
                let height = input.u16()?;
                let primary = input.bool()?;
                let name_len = input.u16()? as usize;
                if name_len > MAX_MONITOR_NAME_BYTES {
                    return malformed(kind, "monitor name is too long");
                }
                let name = std::str::from_utf8(input.take(name_len)?)
                    .map_err(|_| ProtocolError::MalformedPayload {
                        kind,
                        reason: "monitor name is not UTF-8",
                    })?
                    .to_owned();
                monitors.push(MonitorInfo {
                    id,
                    width,
                    height,
                    primary,
                    name,
                });
            }
            Message::MonitorList(monitors)
        }
        MessageKind::CursorKind => Message::CursorKind(
            crate::input::CursorKind::from_id(input.u8()?).ok_or(ProtocolError::MalformedPayload {
                kind,
                reason: "unknown cursor kind",
            })?,
        ),
        MessageKind::Cursor => {
            let acknowledged_sequence = input.u16()?;
            let acknowledged = input.bool()?;
            Message::Cursor(CursorState {
                acknowledged_sequence: acknowledged.then_some(acknowledged_sequence),
                x: input.u16()?,
                y: input.u16()?,
                in_bounds: input.bool()?,
                visible: input.bool()?,
            })
        }
        MessageKind::ConfigAck => Message::ConfigAck {
            session_generation: input.u32()?,
            config_revision: input.u32()?,
            accepted: input.bool()?,
            error_code: input.i32()?,
        },
        MessageKind::NoFrame => Message::NoFrame {
            permit_id: input.u64()?,
            session_generation: input.u32()?,
            config_revision: input.u32()?,
            reason: decode_no_frame_reason(kind, input.u16()?)?,
        },
        MessageKind::EncodedAccessUnit => Message::EncodedAccessUnit(EncodedAccessUnit {
            permit_id: input.u64()?,
            session_generation: input.u32()?,
            config_revision: input.u32()?,
            timestamp_90khz: input.u64()?,
            keyframe: input.bool()?,
            codec: decode_codec(kind, input.u8()?)?,
            width: input.u16()?,
            height: input.u16()?,
            fps: input.u16()?,
            encode_us: input.u32()?,
            data: input.rest().to_vec(),
        }),
        MessageKind::MonitorSwitchResult => Message::MonitorSwitchResult(MonitorSwitchResult {
            session_generation: input.u32()?,
            monitor_id: input.u32()?,
            accepted: input.bool()?,
            width: input.u16()?,
            height: input.u16()?,
            error_code: input.i32()?,
        }),
        MessageKind::InputStats => {
            let session_generation = input.u32()?;
            let received_packets = input.u64()?;
            let accepted_packets = input.u64()?;
            let failed_packets = input.u64()?;
            let dropped_packets = input.u64()?;
            let released_inputs = input.u64()?;
            let requested_events = input.u64()?;
            let applied_events = input.u64()?;
            let last_error_code = input.i32()?;
            let has_last_error_code = input.bool()?;
            let last_sequence = input.u16()?;
            let has_last_sequence = input.bool()?;
            Message::InputStats(InputStatistics {
                session_generation,
                received_packets,
                accepted_packets,
                failed_packets,
                dropped_packets,
                released_inputs,
                requested_events,
                applied_events,
                last_error_code: has_last_error_code.then_some(last_error_code),
                last_sequence: has_last_sequence.then_some(last_sequence),
            })
        }
        MessageKind::RecoverableError => Message::RecoverableError(decode_agent_error(&mut input)?),
        MessageKind::FatalError => Message::FatalError(decode_agent_error(&mut input)?),
        MessageKind::ClipboardText | MessageKind::ClipboardPng => {
            let id = input.u32()?;
            let index = input.u16()?;
            let count = input.u16()?;
            let text = input.rest();
            if count == 0 || count > if kind == MessageKind::ClipboardPng { MAX_PNG_CHUNKS } else { MAX_CLIPBOARD_CHUNKS } || index >= count {
                return malformed(kind, "invalid clipboard chunk position");
            }
            if text.len() > MAX_CLIPBOARD_CHUNK_BYTES {
                return malformed(kind, "clipboard chunk is too large");
            }
            let text = std::str::from_utf8(text)
                .map_err(|_| ProtocolError::MalformedPayload {
                    kind,
                    reason: "clipboard chunk is not UTF-8",
                })?
                .to_owned();
            if kind == MessageKind::ClipboardPng { Message::ClipboardPng { id, index, count, text } }
            else { Message::ClipboardText { id, index, count, text } }
        }
    };
    input.finish()?;
    Ok(message)
}

fn malformed<T>(kind: MessageKind, reason: &'static str) -> Result<T, ProtocolError> {
    Err(ProtocolError::MalformedPayload { kind, reason })
}

fn encode_crop(out: &mut Encoder, crop: Option<CropRect>) {
    match crop {
        None => out.u8(0),
        Some(CropRect::Q16 {
            x,
            y,
            width,
            height,
        }) => {
            out.u8(1);
            out.u16(x);
            out.u16(y);
            out.u16(width);
            out.u16(height);
        }
        Some(CropRect::Pixels {
            x,
            y,
            width,
            height,
        }) => {
            out.u8(2);
            out.u32(x);
            out.u32(y);
            out.u32(width);
            out.u32(height);
        }
    }
}

fn decode_crop(input: &mut Decoder<'_>) -> Result<Option<CropRect>, ProtocolError> {
    Ok(match input.u8()? {
        0 => None,
        1 => Some(CropRect::Q16 {
            x: input.u16()?,
            y: input.u16()?,
            width: input.u16()?,
            height: input.u16()?,
        }),
        2 => Some(CropRect::Pixels {
            x: input.u32()?,
            y: input.u32()?,
            width: input.u32()?,
            height: input.u32()?,
        }),
        _ => return malformed(input.kind, "unknown crop representation"),
    })
}

fn encode_agent_error(out: &mut Encoder, error: &AgentError) {
    out.u16(error.operation as u16);
    out.i32(error.error_code);
    out.u32(error.session_generation);
    out.u32(error.config_revision);
    out.u64(error.permit_id.unwrap_or(0));
    out.bool(error.permit_id.is_some());
}

fn decode_agent_error(input: &mut Decoder<'_>) -> Result<AgentError, ProtocolError> {
    let operation = decode_agent_operation(input.kind, input.u16()?)?;
    let error_code = input.i32()?;
    let session_generation = input.u32()?;
    let config_revision = input.u32()?;
    let permit_id = input.u64()?;
    let has_permit = input.bool()?;
    Ok(AgentError {
        operation,
        error_code,
        session_generation,
        config_revision,
        permit_id: has_permit.then_some(permit_id),
    })
}

fn decode_codec(kind: MessageKind, value: u8) -> Result<Codec, ProtocolError> {
    match value {
        1 => Ok(Codec::H264),
        2 => Ok(Codec::Hevc),
        _ => malformed(kind, "unknown codec"),
    }
}

fn decode_stream_mode(kind: MessageKind, value: u8) -> Result<StreamMode, ProtocolError> {
    match value {
        1 => Ok(StreamMode::Work),
        2 => Ok(StreamMode::Gaming),
        _ => malformed(kind, "unknown stream mode"),
    }
}

fn decode_encoder_preset(kind: MessageKind, value: u8) -> Result<EncoderPreset, ProtocolError> {
    match value {
        1 => Ok(EncoderPreset::P1),
        2 => Ok(EncoderPreset::P2),
        3 => Ok(EncoderPreset::P3),
        4 => Ok(EncoderPreset::P4),
        5 => Ok(EncoderPreset::P5),
        6 => Ok(EncoderPreset::P6),
        7 => Ok(EncoderPreset::P7),
        _ => malformed(kind, "unknown encoder preset"),
    }
}

fn decode_encoder_tuning(kind: MessageKind, value: u8) -> Result<EncoderTuning, ProtocolError> {
    match value {
        1 => Ok(EncoderTuning::UltraLowLatency),
        2 => Ok(EncoderTuning::LowLatency),
        _ => malformed(kind, "unknown encoder tuning"),
    }
}

fn decode_encoder_multipass(
    kind: MessageKind,
    value: u8,
) -> Result<EncoderMultipass, ProtocolError> {
    match value {
        0 => Ok(EncoderMultipass::Disabled),
        1 => Ok(EncoderMultipass::Quarter),
        2 => Ok(EncoderMultipass::Full),
        _ => malformed(kind, "unknown encoder multipass"),
    }
}

fn decode_encoder_aq_mode(kind: MessageKind, value: u8) -> Result<EncoderAqMode, ProtocolError> {
    match value {
        0 => Ok(EncoderAqMode::None),
        1 => Ok(EncoderAqMode::Spatial),
        2 => Ok(EncoderAqMode::Temporal),
        _ => malformed(kind, "unknown encoder AQ mode"),
    }
}

fn decode_agent_operation(kind: MessageKind, value: u16) -> Result<AgentOperation, ProtocolError> {
    match value {
        1 => Ok(AgentOperation::Session),
        2 => Ok(AgentOperation::Desktop),
        3 => Ok(AgentOperation::Capture),
        4 => Ok(AgentOperation::Scale),
        5 => Ok(AgentOperation::Encode),
        6 => Ok(AgentOperation::Input),
        7 => Ok(AgentOperation::Monitor),
        8 => Ok(AgentOperation::Ipc),
        _ => malformed(kind, "unknown agent operation"),
    }
}

fn decode_desktop(kind: MessageKind, value: u8) -> Result<Desktop, ProtocolError> {
    match value {
        0 => Ok(Desktop::Unknown),
        1 => Ok(Desktop::Default),
        2 => Ok(Desktop::Winlogon),
        _ => malformed(kind, "unknown desktop"),
    }
}

fn decode_desktop_reason(
    kind: MessageKind,
    value: u16,
) -> Result<DesktopTransitionReason, ProtocolError> {
    match value {
        1 => Ok(DesktopTransitionReason::Startup),
        2 => Ok(DesktopTransitionReason::DesktopSwitched),
        3 => Ok(DesktopTransitionReason::SessionChanged),
        4 => Ok(DesktopTransitionReason::AgentRestarted),
        _ => malformed(kind, "unknown desktop transition reason"),
    }
}

fn decode_capture_state(kind: MessageKind, value: u8) -> Result<CaptureState, ProtocolError> {
    match value {
        1 => Ok(CaptureState::Active),
        2 => Ok(CaptureState::Unavailable),
        3 => Ok(CaptureState::Recovering),
        _ => malformed(kind, "unknown capture state"),
    }
}

fn decode_no_frame_reason(kind: MessageKind, value: u16) -> Result<NoFrameReason, ProtocolError> {
    match value {
        1 => Ok(NoFrameReason::Timeout),
        2 => Ok(NoFrameReason::DesktopUnavailable),
        3 => Ok(NoFrameReason::Backpressured),
        4 => Ok(NoFrameReason::NoChange),
        _ => malformed(kind, "unknown no-frame reason"),
    }
}

#[derive(Default)]
struct Encoder(Vec<u8>);

impl Encoder {
    fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }
    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.bytes(&value.to_le_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.bytes(&value.to_le_bytes());
    }
    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }
}

struct Decoder<'a> {
    kind: MessageKind,
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    fn new(kind: MessageKind, payload: &'a [u8]) -> Self {
        Self {
            kind,
            remaining: payload,
        }
    }

    fn take(&mut self, size: usize) -> Result<&'a [u8], ProtocolError> {
        if self.remaining.len() < size {
            return malformed(self.kind, "payload is shorter than its message schema");
        }
        let (value, rest) = self.remaining.split_at(size);
        self.remaining = rest;
        Ok(value)
    }

    fn rest(&mut self) -> &'a [u8] {
        let value = self.remaining;
        self.remaining = &[];
        value
    }

    fn finish(self) -> Result<(), ProtocolError> {
        if self.remaining.is_empty() {
            Ok(())
        } else {
            malformed(self.kind, "payload has trailing bytes")
        }
    }

    fn bool(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => malformed(self.kind, "boolean is not 0 or 1"),
        }
    }
    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtocolError> {
        Ok(u16::from_le_bytes(
            self.take(2)?.try_into().expect("two-byte decoder slice"),
        ))
    }
    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four-byte decoder slice"),
        ))
    }
    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("four-byte decoder slice"),
        ))
    }
    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("eight-byte decoder slice"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn cursor_kind_ipc_round_trips_all_native_types_and_rejects_invalid_ids() {
        for id in 0..=14 {
            let message = Message::CursorKind(crate::input::CursorKind::from_id(id).unwrap());
            let mut wire = Vec::new();
            write_frame(&mut wire, 0, 1, &message).unwrap();
            assert_eq!(
                read_frame(&mut Cursor::new(wire)).unwrap().unwrap().message,
                message
            );
        }
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            0,
            1,
            &Message::CursorKind(crate::input::CursorKind::Arrow),
        )
        .unwrap();
        *wire.last_mut().unwrap() = 255;
        assert!(matches!(
            read_frame(&mut Cursor::new(wire)),
            Err(ProtocolError::MalformedPayload {
                kind: MessageKind::CursorKind,
                ..
            })
        ));
    }
    #[test]
    fn representative_messages_round_trip() {
        let messages = [
            Message::RenewLease {
                lease_id: 7,
                ttl_ms: 2_000,
            },
            Message::ConfigureMedia(MediaConfig {
                session_generation: 9,
                config_revision: 2,
                codec: Codec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 8_000_000,
                stream_mode: StreamMode::Gaming,
            }),
            Message::DesktopTransition {
                desktop: Desktop::Winlogon,
                generation: 10,
                input_enabled: true,
                reason: DesktopTransitionReason::DesktopSwitched,
            },
            Message::EncodedAccessUnit(EncodedAccessUnit {
                permit_id: 51,
                session_generation: 10,
                config_revision: 2,
                timestamp_90khz: 123_456,
                keyframe: true,
                codec: Codec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                encode_us: 1_250,
                data: vec![1, 2, 3, 4],
            }),
        ];

        for message in messages {
            let mut wire = Vec::new();
            write_frame(&mut wire, 0xaabb_ccdd, 42, &message).unwrap();
            let decoded = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
            assert_eq!(decoded.flags, 0xaabb_ccdd);
            assert_eq!(decoded.sequence, 42);
            assert_eq!(decoded.message, message);
        }
    }

    #[test]
    fn clean_eof_is_distinct_from_truncated_frames() {
        assert_eq!(
            read_frame(&mut Cursor::new(Vec::<u8>::new())).unwrap(),
            None
        );

        let error = read_frame(&mut Cursor::new(MAGIC[..2].to_vec())).unwrap_err();
        assert!(matches!(error, ProtocolError::TruncatedHeader));

        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            0,
            1,
            &Message::SetBitrate {
                bitrate_bps: 3_000_000,
            },
        )
        .unwrap();
        wire.pop();
        let error = read_frame(&mut Cursor::new(wire)).unwrap_err();
        assert!(matches!(error, ProtocolError::TruncatedPayload { .. }));
    }

    #[test]
    fn malformed_and_oversized_frames_are_rejected_before_payload_allocation() {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_le_bytes());
        header[6..8].copy_from_slice(&(MessageKind::Input as u16).to_le_bytes());
        header[12..16].copy_from_slice(&((MAX_CONTROL_PAYLOAD_BYTES + 1) as u32).to_le_bytes());

        let error = read_frame(&mut Cursor::new(header)).unwrap_err();
        assert!(matches!(
            error,
            ProtocolError::PayloadTooLarge {
                kind: MessageKind::Input,
                ..
            }
        ));

        let mut unknown = header;
        unknown[6..8].copy_from_slice(&u16::MAX.to_le_bytes());
        unknown[12..16].copy_from_slice(&0u32.to_le_bytes());
        let error = read_frame(&mut Cursor::new(unknown)).unwrap_err();
        assert!(matches!(error, ProtocolError::UnknownMessageKind(u16::MAX)));
    }

    #[test]
    fn every_message_schema_round_trips() {
        let messages = vec![
            Message::RenewLease {
                lease_id: u64::MAX,
                ttl_ms: 2_000,
            },
            Message::EncodePermit {
                permit_id: 44,
                session_generation: 3,
                config_revision: 7,
                stream_mode: StreamMode::Work,
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_bps: 3_000_000,
                codec: Codec::H264,
                preset: EncoderPreset::P3,
                tuning: EncoderTuning::UltraLowLatency,
                multipass: EncoderMultipass::Disabled,
                aq_mode: EncoderAqMode::Spatial,
                aq_strength: 8,
                vbv_frames: 1,
                lookahead_depth: 0,
                weighted_prediction: false,
                gop_seconds: 2,
                crop: Some(CropRect::Q16 {
                    x: 100,
                    y: 200,
                    width: 40_000,
                    height: 30_000,
                }),
                force_idr: true,
                allow_idle_snapshot: false,
            },
            Message::ConfigureMedia(MediaConfig {
                session_generation: 3,
                config_revision: 7,
                codec: Codec::H264,
                width: 1280,
                height: 720,
                fps: 30,
                bitrate_bps: 3_000_000,
                stream_mode: StreamMode::Work,
            }),
            Message::SetBitrate {
                bitrate_bps: 4_000_000,
            },
            Message::ForceIdr { generation: 3 },
            Message::Input(vec![0, 1, 2, 255]),
            Message::SelectMonitor { monitor_id: 2 },
            Message::ReleaseAll,
            Message::Shutdown,
            Message::ListMonitors,
            Message::RequestIdleSnapshot {
                permit_id: 45,
                session_generation: 3,
                config_revision: 7,
            },
            Message::Capabilities(Capabilities {
                flags: 0b1011,
                max_width: 1920,
                max_height: 1080,
                max_fps: 60,
                max_bitrate_bps: 40_000_000,
            }),
            Message::DesktopTransition {
                desktop: Desktop::Default,
                generation: 4,
                input_enabled: false,
                reason: DesktopTransitionReason::SessionChanged,
            },
            Message::CaptureHealth {
                state: CaptureState::Recovering,
                error_code: -2005270527,
            },
            Message::MonitorList(vec![MonitorInfo {
                id: 7,
                width: 1920,
                height: 1080,
                primary: true,
                name: "DISPLAY1".into(),
            }]),
            Message::Cursor(CursorState {
                acknowledged_sequence: Some(u16::MAX),
                x: 123,
                y: 456,
                in_bounds: true,
                visible: false,
            }),
            Message::Cursor(CursorState {
                acknowledged_sequence: None,
                x: 0,
                y: 0,
                in_bounds: false,
                visible: true,
            }),
            Message::ConfigAck {
                session_generation: 4,
                config_revision: 8,
                accepted: false,
                error_code: 5,
            },
            Message::NoFrame {
                permit_id: 46,
                session_generation: 4,
                config_revision: 8,
                reason: NoFrameReason::DesktopUnavailable,
            },
            Message::EncodedAccessUnit(EncodedAccessUnit {
                permit_id: 47,
                session_generation: 4,
                config_revision: 8,
                timestamp_90khz: u64::MAX,
                keyframe: false,
                codec: Codec::H264,
                width: 1280,
                height: 720,
                fps: 30,
                encode_us: 3_100,
                data: vec![0, 0, 1, 65, 42],
            }),
            Message::MonitorSwitchResult(MonitorSwitchResult {
                session_generation: 4,
                monitor_id: 7,
                accepted: true,
                width: 1920,
                height: 1080,
                error_code: 0,
            }),
            Message::InputStats(InputStatistics {
                session_generation: 4,
                received_packets: 103,
                accepted_packets: 100,
                failed_packets: 1,
                dropped_packets: 2,
                released_inputs: 1,
                requested_events: 121,
                applied_events: 120,
                last_error_code: Some(5),
                last_sequence: Some(99),
            }),
            Message::RecoverableError(AgentError {
                operation: AgentOperation::Capture,
                error_code: -1,
                session_generation: 4,
                config_revision: 8,
                permit_id: Some(47),
            }),
            Message::FatalError(AgentError {
                operation: AgentOperation::Ipc,
                error_code: 6,
                session_generation: 4,
                config_revision: 8,
                permit_id: None,
            }),
            Message::ClipboardText {
                id: 9,
                index: 1,
                count: 2,
                text: "wörld\r\n".into(),
            },
            Message::ClipboardText {
                id: 10,
                index: 0,
                count: 1,
                text: String::new(),
            },
        ];

        for (sequence, message) in messages.into_iter().enumerate() {
            let mut wire = Vec::new();
            write_frame(&mut wire, 0, sequence as u32, &message).unwrap();
            let frame = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
            assert_eq!(frame.sequence, sequence as u32);
            assert_eq!(frame.message, message);
        }
    }

    #[test]
    fn invalid_magic_version_boolean_and_trailing_bytes_are_rejected() {
        let frame_for = |message: &Message| {
            let mut wire = Vec::new();
            write_frame(&mut wire, 0, 1, message).unwrap();
            wire
        };

        let mut bad_magic = frame_for(&Message::ReleaseAll);
        bad_magic[0] ^= 0xff;
        assert!(matches!(
            read_frame(&mut Cursor::new(bad_magic)).unwrap_err(),
            ProtocolError::InvalidMagic(_)
        ));

        let mut bad_version = frame_for(&Message::ReleaseAll);
        bad_version[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(bad_version)).unwrap_err(),
            ProtocolError::UnsupportedVersion(_)
        ));

        let mut bad_boolean = frame_for(&Message::ConfigAck {
            session_generation: 1,
            config_revision: 1,
            accepted: true,
            error_code: 0,
        });
        bad_boolean[HEADER_LEN + 8] = 2;
        assert!(matches!(
            read_frame(&mut Cursor::new(bad_boolean)).unwrap_err(),
            ProtocolError::MalformedPayload { .. }
        ));

        let mut trailing = frame_for(&Message::ReleaseAll);
        trailing[12..16].copy_from_slice(&1u32.to_le_bytes());
        trailing.push(0);
        assert!(matches!(
            read_frame(&mut Cursor::new(trailing)).unwrap_err(),
            ProtocolError::MalformedPayload { .. }
        ));
    }

    #[test]
    fn writer_enforces_control_and_media_limits() {
        let error = write_frame(
            &mut Vec::new(),
            0,
            1,
            &Message::Input(vec![0; MAX_CONTROL_PAYLOAD_BYTES + 1]),
        )
        .unwrap_err();
        assert!(matches!(error, ProtocolError::PayloadTooLarge { .. }));

        let metadata_bytes = 8 + 4 + 4 + 8 + 1 + 1 + 2 + 2 + 2 + 4;
        let maximum_data = MAX_MEDIA_PAYLOAD_BYTES - metadata_bytes;
        write_frame(
            &mut Vec::new(),
            0,
            2,
            &Message::EncodedAccessUnit(EncodedAccessUnit {
                permit_id: 1,
                session_generation: 1,
                config_revision: 1,
                timestamp_90khz: 0,
                keyframe: true,
                codec: Codec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                encode_us: 500,
                data: vec![0; maximum_data],
            }),
        )
        .unwrap();

        let error = write_frame(
            &mut Vec::new(),
            0,
            3,
            &Message::EncodedAccessUnit(EncodedAccessUnit {
                permit_id: 2,
                session_generation: 1,
                config_revision: 1,
                timestamp_90khz: 0,
                keyframe: true,
                codec: Codec::Hevc,
                width: 1920,
                height: 1080,
                fps: 60,
                encode_us: 500,
                data: vec![0; maximum_data + 1],
            }),
        )
        .unwrap_err();
        assert!(matches!(error, ProtocolError::PayloadTooLarge { .. }));
    }

    #[test]
    fn permit_results_echo_identity_so_stale_results_can_be_discarded() {
        let old_identity = PermitIdentity {
            permit_id: 40,
            session_generation: 3,
            config_revision: 8,
        };
        let current_identity = PermitIdentity {
            permit_id: 41,
            session_generation: 4,
            config_revision: 9,
        };
        let old_result = Message::NoFrame {
            permit_id: old_identity.permit_id,
            session_generation: old_identity.session_generation,
            config_revision: old_identity.config_revision,
            reason: NoFrameReason::NoChange,
        };

        let mut wire = Vec::new();
        write_frame(&mut wire, 0, 1, &old_result).unwrap();
        let decoded = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
        assert_eq!(decoded.message.permit_identity(), Some(old_identity));
        assert_ne!(decoded.message.permit_identity(), Some(current_identity));

        let access_unit = Message::EncodedAccessUnit(EncodedAccessUnit {
            permit_id: current_identity.permit_id,
            session_generation: current_identity.session_generation,
            config_revision: current_identity.config_revision,
            timestamp_90khz: 90_000,
            keyframe: true,
            codec: Codec::Hevc,
            width: 1920,
            height: 1080,
            fps: 60,
            encode_us: 2_300,
            data: vec![1, 2, 3],
        });
        let mut wire = Vec::new();
        write_frame(&mut wire, 0, 2, &access_unit).unwrap();
        let decoded = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
        assert_eq!(decoded.message.permit_identity(), Some(current_identity));
    }

    #[test]
    fn encode_permit_supports_full_q16_and_pixel_crops() {
        let crops = [
            None,
            Some(CropRect::Q16 {
                x: 1_000,
                y: 2_000,
                width: 30_000,
                height: 40_000,
            }),
            Some(CropRect::Pixels {
                x: 320,
                y: 180,
                width: 1280,
                height: 720,
            }),
        ];

        for (sequence, crop) in crops.into_iter().enumerate() {
            let permit = Message::EncodePermit {
                permit_id: sequence as u64 + 1,
                session_generation: 5,
                config_revision: 11,
                stream_mode: StreamMode::Gaming,
                width: 1920,
                height: 1080,
                fps: 60,
                bitrate_bps: 8_000_000,
                codec: Codec::Hevc,
                preset: EncoderPreset::P7,
                tuning: EncoderTuning::LowLatency,
                multipass: EncoderMultipass::Full,
                aq_mode: EncoderAqMode::Temporal,
                aq_strength: 10,
                vbv_frames: 4,
                lookahead_depth: 2,
                weighted_prediction: true,
                gop_seconds: 4,
                crop,
                force_idr: sequence == 0,
                allow_idle_snapshot: sequence != 0,
            };
            let mut wire = Vec::new();
            write_frame(&mut wire, 0, sequence as u32, &permit).unwrap();
            let decoded = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
            assert_eq!(decoded.message, permit);
        }
    }
}
