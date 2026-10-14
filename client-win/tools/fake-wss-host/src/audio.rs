//! Simulated PC sound for the fake host: a 440 Hz stereo tone, Opus-encoded in
//! 10 ms frames and framed as BPA1 packets (`docs/architecture/audio.md`).
//! The lane is plain logic plus the encoder: the caller passes `now`, so the
//! scripted loss and reorder are deterministic and the tests need no clock.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub const MAGIC: [u8; 4] = *b"BPA1";
pub const VERSION: u8 = 1;
pub const HEADER_BYTES: usize = 28;
pub const MAX_PAYLOAD_BYTES: usize = 1500;
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;
pub const FRAME_SAMPLES: usize = 480;
/// Interleaved samples in one 10 ms stereo frame.
pub const FRAME_LEN: usize = FRAME_SAMPLES * 2;
pub const FRAME_PERIOD: Duration = Duration::from_millis(10);
pub const TONE_HZ: f64 = 440.0;
const TONE_AMPLITUDE: f64 = 0.25;
const BITRATE_BPS: i32 = 128_000;
const CODEC_OPUS: u8 = 1;
const FLAG_DISCONTINUITY: u8 = 0x01;
/// Packets the ready queue holds; the oldest is dropped when it is full.
pub const QUEUE_DEPTH: usize = 10;
/// Most packets one `produce` call makes; a longer stall resyncs instead.
const MAX_CATCH_UP: u32 = 10;
/// The `audio` value on a protocol-4 start that opts in. Any other value does not.
pub const OPT_IN_VALUE: u64 = 1;
/// Largest accepted `--audio-*-every` value.
pub const MAX_EVERY: u64 = 1_000_000;

/// True only for the exact opt-in value; absent, other and non-integer values do not opt in.
pub fn opts_in(value: Option<u64>) -> bool {
    value == Some(OPT_IN_VALUE)
}

/// One BPA1 packet before framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub seq: u32,
    pub capture_ts_us: u64,
    pub discontinuity: bool,
    pub payload: Vec<u8>,
}

/// Serialises one packet: 28-byte big-endian header followed by the Opus payload.
pub fn encode_packet(packet: &Packet) -> Result<Vec<u8>, &'static str> {
    let len = packet.payload.len();
    if !(1..=MAX_PAYLOAD_BYTES).contains(&len) {
        return Err("audio payload outside 1..=1500 bytes");
    }
    let mut out = Vec::with_capacity(HEADER_BYTES + len);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(if packet.discontinuity {
        FLAG_DISCONTINUITY
    } else {
        0
    });
    out.push(CODEC_OPUS);
    out.push(CHANNELS);
    out.extend_from_slice(&SAMPLE_RATE.to_be_bytes());
    out.extend_from_slice(&packet.seq.to_be_bytes());
    out.extend_from_slice(&packet.capture_ts_us.to_be_bytes());
    out.extend_from_slice(&(FRAME_SAMPLES as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&packet.payload);
    Ok(out)
}

/// Parses one packet. Every header field is checked against the contract;
/// a truncated header or a payload outside 1..=1500 bytes is an error.
pub fn decode_packet(bytes: &[u8]) -> Result<Packet, &'static str> {
    if bytes.len() < HEADER_BYTES {
        return Err("audio packet truncated");
    }
    if bytes[0..4] != MAGIC {
        return Err("audio packet has the wrong magic");
    }
    if bytes[4] != VERSION {
        return Err("unsupported audio version");
    }
    let flags = bytes[5];
    if flags & !FLAG_DISCONTINUITY != 0 {
        return Err("unknown audio flags");
    }
    if bytes[6] != CODEC_OPUS {
        return Err("unsupported audio codec");
    }
    if bytes[7] != CHANNELS {
        return Err("unsupported audio channels");
    }
    let be32 =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let be16 = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
    if be32(8) != SAMPLE_RATE {
        return Err("unsupported audio sample rate");
    }
    let mut ts = [0u8; 8];
    ts.copy_from_slice(&bytes[16..24]);
    if be16(24) as usize != FRAME_SAMPLES {
        return Err("unsupported audio frame samples");
    }
    if be16(26) != 0 {
        return Err("nonzero reserved audio field");
    }
    let payload = &bytes[HEADER_BYTES..];
    if !(1..=MAX_PAYLOAD_BYTES).contains(&payload.len()) {
        return Err("audio payload outside 1..=1500 bytes");
    }
    Ok(Packet {
        seq: be32(12),
        capture_ts_us: u64::from_be_bytes(ts),
        discontinuity: flags & FLAG_DISCONTINUITY != 0,
        payload: payload.to_vec(),
    })
}

/// The 440 Hz tone, sample by sample, identical on both channels. 440 cycles
/// fit in exactly 48 000 samples, so the phase is kept modulo one second.
#[derive(Debug, Default)]
pub struct Tone {
    next: u64,
}

impl Tone {
    /// The next 10 ms of tone, interleaved stereo.
    pub fn next_frame(&mut self) -> Vec<f32> {
        let mut out = Vec::with_capacity(FRAME_LEN);
        for _ in 0..FRAME_SAMPLES {
            let position = (self.next % u64::from(SAMPLE_RATE)) as f64;
            let phase = 2.0 * std::f64::consts::PI * TONE_HZ * position / f64::from(SAMPLE_RATE);
            let value = (phase.sin() * TONE_AMPLITUDE) as f32;
            out.push(value);
            out.push(value);
            self.next += 1;
        }
        out
    }
}

/// Opus at the host's settings: Application Audio, 128 kbps VBR, stereo,
/// no in-band FEC, no DTX. One call encodes one 10 ms frame.
pub struct ToneEncoder {
    inner: opus::Encoder,
    scratch: Vec<u8>,
    tone: Tone,
}

impl ToneEncoder {
    pub fn new() -> Result<Self> {
        let mut inner = opus::Encoder::new(
            SAMPLE_RATE,
            opus::Channels::Stereo,
            opus::Application::Audio,
        )
        .context("create opus encoder")?;
        inner
            .set_bitrate(opus::Bitrate::Bits(BITRATE_BPS))
            .context("set opus bitrate")?;
        inner.set_vbr(true).context("set opus vbr")?;
        inner.set_inband_fec(false).context("disable opus fec")?;
        inner.set_dtx(false).context("disable opus dtx")?;
        Ok(Self {
            inner,
            scratch: vec![0; MAX_PAYLOAD_BYTES],
            tone: Tone::default(),
        })
    }

    /// The Opus payload for the next 10 ms of tone.
    pub fn next_payload(&mut self) -> Result<Vec<u8>> {
        let pcm = self.tone.next_frame();
        let len = self
            .inner
            .encode_float(&pcm, &mut self.scratch)
            .context("encode opus frame")?;
        Ok(self.scratch[..len].to_vec())
    }
}

/// Deterministic wire faults, counted over produced packets from 1. Packet N
/// is dropped for `loss_every` N: its sequence number is used, the next packet
/// carries no discontinuity, so the receiver sees a plain gap. For
/// `reorder_every` N, packet N is sent after the packet that follows it, also
/// unflagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Script {
    pub loss_every: Option<u64>,
    pub reorder_every: Option<u64>,
}

impl Script {
    pub fn new(loss_every: Option<u64>, reorder_every: Option<u64>) -> Result<Self, String> {
        for (name, value) in [
            ("--audio-loss-every", loss_every),
            ("--audio-reorder-every", reorder_every),
        ] {
            if let Some(n) = value {
                if !(2..=MAX_EVERY).contains(&n) {
                    return Err(format!("{name} must be between 2 and {MAX_EVERY}"));
                }
            }
        }
        Ok(Self {
            loss_every,
            reorder_every,
        })
    }

    pub fn loses(&self, index: u64) -> bool {
        self.loss_every.is_some_and(|n| index.is_multiple_of(n))
    }

    pub fn reorders(&self, index: u64) -> bool {
        self.reorder_every.is_some_and(|n| index.is_multiple_of(n))
    }
}

/// Audio-specific report counts. Accounting: `packets_encoded` equals
/// `packets_sent + lost_by_script + queue_dropped + send_failed + discarded`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioCounters {
    /// Accepted starts that carried `audio:1`, whatever the video outcome.
    pub opt_in_starts: u64,
    /// Times the lane began producing: the first start of an epoch, and each
    /// restart after a stop, a media reconnect or a resync.
    pub starts: u64,
    pub stops: u64,
    /// Packets that were due and encoded (the script index counts these).
    pub packets_encoded: u64,
    pub packets_sent: u64,
    pub bytes_sent: u64,
    /// Sent packets that carry the discontinuity flag.
    pub discontinuities_sent: u64,
    pub lost_by_script: u64,
    pub reordered: u64,
    /// Oldest packets dropped because the ready queue was full.
    pub queue_dropped: u64,
    /// Packets the media socket refused.
    pub send_failed: u64,
    /// Packets still queued or held when the lane stopped.
    pub discarded: u64,
    /// Opus refused a frame; no packet exists, so it is outside the accounting.
    pub encode_errors: u64,
    /// The loop fell more than 100 ms behind and skipped ahead.
    pub resyncs: u64,
}

/// The audio lane for one run. It produces 10 ms packets while active and
/// hands them out in order; the hub decides when a lane is active.
pub struct AudioLane {
    script: Script,
    run_start: Instant,
    encoder: ToneEncoder,
    counters: AudioCounters,
    active: bool,
    /// Capture time of the next packet while active.
    next_due: Instant,
    seq: u32,
    /// A host-side gap happened (start, queue drop, refused send, resync,
    /// encode error); the next packet to be queued carries the discontinuity.
    /// A scripted loss is network loss and does not set this.
    gap: bool,
    /// A packet held back by the reorder script, sent after the next one.
    pending: Option<Packet>,
    ready: VecDeque<Packet>,
}

impl AudioLane {
    pub fn new(script: Script, run_start: Instant) -> Result<Self> {
        Ok(Self {
            script,
            run_start,
            encoder: ToneEncoder::new()?,
            counters: AudioCounters::default(),
            active: false,
            next_due: run_start,
            seq: 0,
            gap: true,
            pending: None,
            ready: VecDeque::new(),
        })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn counters(&self) -> &AudioCounters {
        &self.counters
    }

    pub fn note_opt_in(&mut self) {
        self.counters.opt_in_starts += 1;
    }

    /// Begin producing from `now`. The first packet is a discontinuity.
    pub fn start(&mut self, now: Instant) {
        if self.active {
            return;
        }
        self.active = true;
        self.next_due = now;
        self.gap = true;
        self.counters.starts += 1;
    }

    /// Stop producing. Packets not yet delivered are discarded and counted.
    pub fn stop(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.counters.stops += 1;
        let held = usize::from(self.pending.take().is_some());
        self.counters.discarded += (self.ready.len() + held) as u64;
        self.ready.clear();
    }

    /// Produce every packet due by `now`, at most `MAX_CATCH_UP` of them.
    pub fn produce(&mut self, now: Instant) {
        if !self.active {
            return;
        }
        if now.saturating_duration_since(self.next_due) > FRAME_PERIOD * MAX_CATCH_UP {
            self.next_due = now;
            self.gap = true;
            self.counters.resyncs += 1;
        }
        for _ in 0..MAX_CATCH_UP {
            if self.next_due > now {
                break;
            }
            let due = self.next_due;
            self.next_due += FRAME_PERIOD;
            self.produce_one(due);
        }
    }

    fn produce_one(&mut self, due: Instant) {
        let payload = match self.encoder.next_payload() {
            Ok(payload) => payload,
            Err(_) => {
                self.counters.encode_errors += 1;
                self.gap = true;
                return;
            }
        };
        self.counters.packets_encoded += 1;
        let index = self.counters.packets_encoded;
        // A lost packet keeps its sequence number, so the receiver sees the gap.
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let packet = Packet {
            seq,
            capture_ts_us: due.saturating_duration_since(self.run_start).as_micros() as u64,
            discontinuity: false,
            payload,
        };
        if self.script.loses(index) {
            // Network loss: the host cannot see it, so the next packet is not
            // flagged. The receiver sees only the sequence gap and conceals it.
            self.counters.lost_by_script += 1;
            return;
        }
        if self.script.reorders(index) && self.pending.is_none() {
            self.counters.reordered += 1;
            self.pending = Some(packet);
            return;
        }
        self.enqueue(packet);
        if let Some(held) = self.pending.take() {
            self.enqueue(held);
        }
    }

    /// Queue a packet for sending. The flag goes on the first packet sent
    /// after a gap, not on the packet that was produced after the gap.
    fn enqueue(&mut self, mut packet: Packet) {
        packet.discontinuity = std::mem::take(&mut self.gap);
        if self.ready.len() >= QUEUE_DEPTH {
            self.ready.pop_front();
            self.counters.queue_dropped += 1;
            self.gap = true;
        }
        self.ready.push_back(packet);
    }

    /// Hands every queued packet to `send`, in order. `send` returns false
    /// when the socket refused the packet.
    pub fn deliver(&mut self, mut send: impl FnMut(Vec<u8>) -> bool) {
        for packet in std::mem::take(&mut self.ready) {
            let discontinuity = packet.discontinuity;
            let bytes = match encode_packet(&packet) {
                Ok(bytes) => bytes,
                Err(_) => {
                    self.counters.send_failed += 1;
                    self.gap = true;
                    continue;
                }
            };
            let len = bytes.len() as u64;
            if send(bytes) {
                self.counters.packets_sent += 1;
                self.counters.bytes_sent += len;
                if discontinuity {
                    self.counters.discontinuities_sent += 1;
                }
            } else {
                self.counters.send_failed += 1;
                self.gap = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../fixtures/audio-contract.json");

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
            .collect()
    }

    fn packet(seq: u32, discontinuity: bool) -> Packet {
        Packet {
            seq,
            capture_ts_us: 1_000,
            discontinuity,
            payload: vec![0xfc, 0xff, 0xfe],
        }
    }

    /// Delivered sequence numbers and flags, in send order.
    fn delivered(lane: &mut AudioLane) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        lane.deliver(|bytes| {
            let p = decode_packet(&bytes).expect("own packet decodes");
            out.push((p.seq, p.discontinuity));
            true
        });
        out
    }

    fn lane(script: Script) -> (AudioLane, Instant) {
        let t0 = Instant::now();
        (AudioLane::new(script, t0).expect("encoder"), t0)
    }

    #[test]
    fn packet_round_trips_every_field() {
        let p = Packet {
            seq: u32::MAX,
            capture_ts_us: u64::MAX,
            discontinuity: true,
            payload: vec![1, 2, 3],
        };
        let bytes = encode_packet(&p).expect("encode");
        assert_eq!(bytes.len(), HEADER_BYTES + 3);
        assert_eq!(&bytes[0..4], b"BPA1");
        assert_eq!(decode_packet(&bytes).expect("decode"), p);
    }

    #[test]
    fn packet_limits_are_inclusive() {
        let at = |len: usize| Packet {
            payload: vec![0xab; len],
            ..packet(1, false)
        };
        assert!(encode_packet(&at(MAX_PAYLOAD_BYTES)).is_ok());
        assert!(encode_packet(&at(MAX_PAYLOAD_BYTES + 1)).is_err());
        assert!(encode_packet(&at(0)).is_err());
        let good = encode_packet(&packet(1, false)).expect("encode");
        assert!(
            decode_packet(&good[..HEADER_BYTES]).is_err(),
            "empty payload"
        );
        assert!(
            decode_packet(&good[..HEADER_BYTES - 1]).is_err(),
            "truncated"
        );
    }

    #[test]
    fn every_header_field_is_checked_on_its_own() {
        let good = unhex("42504131010001020000bb800000000700000000075bcd1501e00000fcfffe");
        assert!(decode_packet(&good).is_ok());
        for (offset, bad) in [(4, 2u8), (5, 0x02), (6, 2), (7, 1), (11, 0x01)] {
            let mut bytes = good.clone();
            bytes[offset] = bad;
            assert!(decode_packet(&bytes).is_err(), "byte {offset} = {bad}");
        }
        let mut bytes = good.clone();
        bytes[26] = 1;
        assert!(decode_packet(&bytes).is_err(), "reserved byte must be zero");
    }

    #[test]
    fn packets_match_the_shared_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).expect("fixture json");
        let mut accepted = 0;
        let mut rejected = 0;
        for entry in fixture["packets"].as_array().expect("packets") {
            let name = entry["name"].as_str().expect("name");
            let bytes = match entry["hex"].as_str() {
                Some(hex) => unhex(hex),
                None => {
                    let mut bytes = unhex(entry["hexPrefix"].as_str().expect("prefix"));
                    let payload = entry["payloadBytes"].as_u64().expect("payload") as usize;
                    bytes.extend(std::iter::repeat_n(0u8, payload));
                    bytes
                }
            };
            if entry["accepted"].as_bool().expect("accepted") {
                accepted += 1;
                let p = decode_packet(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!(p.seq as u64, entry["seq"].as_u64().expect("seq"), "{name}");
                assert_eq!(
                    p.discontinuity,
                    entry["discontinuity"].as_bool().expect("flag")
                );
                assert_eq!(encode_packet(&p).expect("re-encode"), bytes, "{name}");
            } else {
                rejected += 1;
                assert!(decode_packet(&bytes).is_err(), "{name} must be rejected");
            }
        }
        assert_eq!((accepted, rejected), (2, 11), "fixture changed");
    }

    #[test]
    fn tone_is_440_hz_stereo_and_repeats_each_second() {
        let mut tone = Tone::default();
        let mut second = Vec::new();
        for _ in 0..100 {
            second.extend(tone.next_frame());
        }
        assert_eq!(second.len(), 2 * SAMPLE_RATE as usize);
        let left: Vec<f32> = second.iter().step_by(2).copied().collect();
        assert!(second.chunks(2).all(|lr| lr[0] == lr[1]), "channels match");
        let crossings = left
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!((878..=882).contains(&crossings), "{crossings} crossings");
        assert!(left.iter().all(|s| s.abs() <= 0.25 + 1e-6));
        let mut again = Tone::default();
        let mut repeat = Vec::new();
        for _ in 0..100 {
            repeat.extend(again.next_frame());
        }
        assert_eq!(second, repeat, "deterministic");
    }

    #[test]
    fn encoder_packets_decode_to_480_stereo_samples() {
        let mut enc = ToneEncoder::new().expect("encoder");
        let mut dec = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo).expect("decoder");
        let mut out = vec![0f32; FRAME_LEN];
        let mut energy = false;
        for _ in 0..20 {
            let payload = enc.next_payload().expect("encode");
            assert!(!payload.is_empty() && payload.len() <= MAX_PAYLOAD_BYTES);
            let n = dec.decode_float(&payload, &mut out, false).expect("decode");
            assert_eq!(n, FRAME_SAMPLES);
            energy |= out.iter().any(|s| s.abs() > 0.01);
        }
        assert!(energy, "decoded tone is not silent");
    }

    #[test]
    fn script_refuses_out_of_range_and_fires_on_multiples() {
        assert!(Script::new(Some(1), None).is_err());
        assert!(Script::new(None, Some(0)).is_err());
        assert!(Script::new(Some(MAX_EVERY + 1), None).is_err());
        let script = Script::new(Some(37), Some(23)).expect("valid");
        assert!(script.loses(37) && script.loses(74) && !script.loses(36));
        assert!(script.reorders(23) && script.reorders(46) && !script.reorders(24));
        assert!(!Script::default().loses(1));
    }

    #[test]
    fn only_the_exact_opt_in_value_opts_in() {
        assert!(opts_in(Some(1)));
        for other in [None, Some(0), Some(2)] {
            assert!(!opts_in(other), "{other:?}");
        }
    }

    #[test]
    fn a_run_starts_with_a_discontinuity_and_counts_every_packet() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(25));
        assert_eq!(lane.counters().packets_encoded, 3, "due at 0, 10 and 20 ms");
        let got = delivered(&mut lane);
        assert_eq!(got, vec![(0, true), (1, false), (2, false)]);
        assert_eq!(lane.counters().packets_sent, 3);
        assert_eq!(lane.counters().discontinuities_sent, 1);
    }

    #[test]
    fn loss_drops_packets_but_keeps_their_sequence_numbers() {
        let (mut lane, t0) = lane(Script::new(Some(2), None).expect("script"));
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(35));
        assert_eq!(lane.counters().lost_by_script, 2);
        // Packets 2 and 4 are lost; the survivors carry seq 0 and 2. Only the
        // start is flagged: the loss is not visible to the host.
        assert_eq!(delivered(&mut lane), vec![(0, true), (2, false)]);
        assert_eq!(
            lane.counters().lost_by_script,
            lane.counters().packets_encoded / 2
        );
    }

    #[test]
    fn a_scripted_loss_is_an_unflagged_sequence_gap() {
        let (mut lane, t0) = lane(Script::new(Some(3), None).expect("script"));
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(50));
        // Six packets are due. Packets 3 and 6 are lost; their seq 2 and 5 are
        // missing from the wire and nothing after them is flagged.
        assert_eq!(lane.counters().packets_encoded, 6);
        assert_eq!(lane.counters().lost_by_script, 2);
        assert_eq!(
            delivered(&mut lane),
            vec![(0, true), (1, false), (3, false), (4, false)]
        );
        assert_eq!(lane.counters().discontinuities_sent, 1);
        assert_eq!(
            lane.counters().queue_dropped + lane.counters().send_failed,
            0
        );
    }

    #[test]
    fn a_host_side_gap_after_a_loss_is_still_flagged() {
        let (mut lane, t0) = lane(Script::new(Some(2), None).expect("script"));
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(15));
        // Packet 2 (seq 1) is lost on the wire, unflagged; packet 1 is sent.
        assert_eq!(delivered(&mut lane), vec![(0, true)]);
        // Packet 3 (seq 2) is refused by the socket: a host-side gap.
        lane.produce(t0 + Duration::from_millis(25));
        lane.deliver(|_| false);
        assert_eq!(lane.counters().send_failed, 1);
        // Packet 4 (seq 3) is lost on the wire; packet 5 (seq 4) carries the flag.
        lane.produce(t0 + Duration::from_millis(45));
        assert_eq!(lane.counters().lost_by_script, 2);
        assert_eq!(delivered(&mut lane), vec![(4, true)]);
    }

    #[test]
    fn reorder_sends_the_held_packet_after_the_next_one_unflagged() {
        let (mut lane, t0) = lane(Script::new(None, Some(2)).expect("script"));
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(25));
        assert_eq!(lane.counters().reordered, 1);
        // The held seq 1 goes out after seq 2 and is not flagged.
        assert_eq!(
            delivered(&mut lane),
            vec![(0, true), (2, false), (1, false)]
        );
        assert_eq!(lane.counters().discontinuities_sent, 1);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_marks_the_next_packet() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(95));
        lane.produce(t0 + Duration::from_millis(195));
        assert_eq!(lane.counters().packets_encoded, 20);
        assert_eq!(lane.counters().queue_dropped, 10);
        assert_eq!(lane.counters().resyncs, 0);
        let got = delivered(&mut lane);
        assert_eq!(got.len(), QUEUE_DEPTH);
        assert_eq!(got[0].0, 10, "the oldest ten were dropped");
    }

    #[test]
    fn a_long_stall_resyncs_and_marks_a_discontinuity() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(500));
        assert_eq!(lane.counters().resyncs, 1);
        // The resync moves the schedule to `now`, so the backlog is dropped
        // rather than sent as a burst: one packet is due at `now`.
        assert_eq!(lane.counters().packets_encoded, 1);
        assert!(delivered(&mut lane)[0].1);
    }

    #[test]
    fn catch_up_is_capped_per_call_just_inside_the_resync_threshold() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        // Exactly MAX_CATCH_UP periods behind is not more than the threshold, so
        // there is no resync, but one call makes at most MAX_CATCH_UP packets.
        lane.produce(t0 + FRAME_PERIOD * MAX_CATCH_UP);
        assert_eq!(lane.counters().resyncs, 0);
        assert_eq!(lane.counters().packets_encoded, MAX_CATCH_UP as u64);
        // The remaining packet is due at the same instant and is made next call.
        lane.produce(t0 + FRAME_PERIOD * MAX_CATCH_UP);
        assert_eq!(lane.counters().packets_encoded, MAX_CATCH_UP as u64 + 1);
        assert_eq!(lane.counters().resyncs, 0);
    }

    #[test]
    fn stop_discards_what_was_queued_and_a_restart_is_a_discontinuity() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(25));
        lane.stop();
        assert!(!lane.is_active());
        assert_eq!(lane.counters().discarded, 3);
        lane.produce(t0 + Duration::from_millis(100));
        assert_eq!(
            lane.counters().packets_encoded,
            3,
            "nothing produced while stopped"
        );
        lane.start(t0 + Duration::from_millis(100));
        lane.produce(t0 + Duration::from_millis(100));
        assert_eq!(delivered(&mut lane), vec![(3, true)]);
        let c = lane.counters();
        assert_eq!(
            c.packets_encoded,
            c.packets_sent + c.lost_by_script + c.queue_dropped + c.send_failed + c.discarded
        );
        assert_eq!((c.starts, c.stops), (2, 1));
    }

    #[test]
    fn a_refused_send_counts_and_marks_the_next_packet() {
        let (mut lane, t0) = lane(Script::default());
        lane.start(t0);
        lane.produce(t0 + Duration::from_millis(15));
        lane.deliver(|_| false);
        assert_eq!(lane.counters().send_failed, 2);
        lane.produce(t0 + Duration::from_millis(25));
        assert_eq!(delivered(&mut lane), vec![(2, true)]);
    }
}
