use super::{
    depacketize::{self, AccessUnit, Depacketizer, RtpPacketRef},
    stats::RtpTracker,
};
use crate::net::Codec;
use std::{collections::VecDeque, time::Instant};
use str0m::{
    media::Pt,
    rtp::{RtpPacket, Ssrc},
};

pub struct Unit {
    pub au: AccessUnit,
    pub received_at: Instant,
    pub seq: u32,
}
pub struct Ingress {
    video: Option<(Ssrc, Pt, Codec, Depacketizer)>,
    receipts: VecDeque<(u32, Instant)>,
    sequence: u32,
    pub last_video: Option<Instant>,
    pub video_tracker: RtpTracker,
    pub audio_tracker: RtpTracker,
}
impl Ingress {
    pub fn new(now: Instant) -> Self {
        Self {
            video: None,
            receipts: VecDeque::new(),
            sequence: 0,
            last_video: None,
            video_tracker: RtpTracker::new(now),
            audio_tracker: RtpTracker::new(now),
        }
    }
    pub fn push(&mut self, codec: Codec, packet: &RtpPacket) -> (Vec<Unit>, bool, u64) {
        let changed = self.video.as_ref().is_none_or(|(ssrc, pt, current, _)| {
            *ssrc != packet.header.ssrc || *pt != packet.header.payload_type || *current != codec
        });
        if changed {
            if let Some((_, _, _, dep)) = &mut self.video {
                dep.reset();
            }
            self.video = Some((
                packet.header.ssrc,
                packet.header.payload_type,
                codec,
                Depacketizer::new(match codec {
                    Codec::H264 => depacketize::Codec::H264,
                    Codec::Hevc => depacketize::Codec::H265,
                }),
            ));
            self.receipts.clear();
        }
        self.last_video = Some(packet.timestamp);
        let timestamp = packet.header.timestamp;
        if !self.receipts.iter().any(|(ts, _)| *ts == timestamp) {
            if self.receipts.len() >= 64 {
                self.receipts.pop_front();
            }
            self.receipts.push_back((timestamp, packet.timestamp));
        }
        let dep = &mut self.video.as_mut().unwrap().3;
        let before = dep.stats();
        let mut aus = Vec::new();
        let malformed = dep
            .push(
                RtpPacketRef {
                    sequence_number: *packet.seq_no as u16,
                    timestamp,
                    marker: packet.header.marker,
                    payload: &packet.payload,
                },
                &mut aus,
            )
            .is_err();
        let after = dep.stats();
        let dropped = after.partial_discarded - before.partial_discarded
            + after.oversized_discarded
            - before.oversized_discarded
            + u64::from(malformed);
        let units = self.units(aus, packet.timestamp);
        (units, changed, dropped)
    }
    fn units(&mut self, aus: Vec<AccessUnit>, fallback: Instant) -> Vec<Unit> {
        aus.into_iter()
            .map(|au| {
                let at = self
                    .receipts
                    .iter()
                    .position(|(ts, _)| *ts == au.rtp_timestamp)
                    .and_then(|index| self.receipts.remove(index))
                    .map_or(fallback, |(_, at)| at);
                let seq = self.sequence;
                self.sequence = self.sequence.wrapping_add(1);
                Unit {
                    au,
                    received_at: at,
                    seq,
                }
            })
            .collect()
    }
    pub fn flush(&mut self) -> (Vec<Unit>, u64) {
        let mut aus = Vec::new();
        let mut dropped = 0;
        if let Some((_, _, _, dep)) = &mut self.video {
            let before = dep.stats();
            dep.flush(&mut aus);
            let after = dep.stats();
            dropped = after.partial_discarded - before.partial_discarded
                + after.oversized_discarded
                - before.oversized_discarded;
        }
        self.last_video = None;
        (self.units(aus, Instant::now()), dropped)
    }
}
