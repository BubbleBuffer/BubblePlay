//! Strict BPA1 framing. Recognition deliberately does not imply validity.
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_SAMPLES: usize = 480;
pub const HEADER_BYTES: usize = 28;
pub const MAX_PAYLOAD_BYTES: usize = 1500;

#[derive(Clone, Debug)]
pub struct Packet {
    pub seq: u32,
    pub capture_ts_us: u64,
    pub discontinuity: bool,
    pub payload: Vec<u8>,
}

pub fn is_audio_packet(bytes: &[u8]) -> bool {
    bytes.starts_with(b"BPA1")
}

pub fn parse_packet(bytes: &[u8]) -> Option<Packet> {
    if !(HEADER_BYTES + 1..=HEADER_BYTES + MAX_PAYLOAD_BYTES).contains(&bytes.len())
        || !is_audio_packet(bytes)
        || bytes[4] != 1
        || bytes[5] & !1 != 0
        || bytes[6] != 1
        || bytes[7] != 2
        || bytes[8..12] != SAMPLE_RATE.to_be_bytes()
        || bytes[24..26] != (FRAME_SAMPLES as u16).to_be_bytes()
        || bytes[26..28] != [0, 0]
    {
        return None;
    }
    Some(Packet {
        seq: u32::from_be_bytes(bytes[12..16].try_into().ok()?),
        capture_ts_us: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
        discontinuity: bytes[5] & 1 != 0,
        payload: bytes[HEADER_BYTES..].to_vec(),
    })
}
