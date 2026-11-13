//! Receive offer follows receiver.js:1933-1965: video, Opus audio, then the two
//! DCEP channels. The browser assigns channel IDs; it does not pin any IDs or
//! fmtp in JavaScript. str0m owns SDP IDs and the RFC codec parameters. HEVC
//! precedes H.264 per the native step-5 contract; both remain negotiable.
use super::channels::{CONTROL_LABEL, MOTION_LABEL};
use std::time::{Duration, Instant};
use str0m::{
    change::SdpPendingOffer,
    channel::{ChannelConfig, Reliability},
    media::{Direction, MediaKind, Mid},
    Rtc,
};

pub fn channel_configs() -> [ChannelConfig; 2] {
    [
        ChannelConfig {
            label: CONTROL_LABEL.into(),
            ordered: true,
            reliability: Reliability::Reliable,
            ..Default::default()
        },
        ChannelConfig {
            label: MOTION_LABEL.into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits { retransmits: 0 },
            ..Default::default()
        },
    ]
}
pub fn rtc(now: Instant, hevc: bool) -> Rtc {
    Rtc::builder()
        .clear_codecs()
        .enable_h265(hevc)
        .enable_h264(true)
        .enable_opus(true)
        .set_rtp_mode(true)
        .set_stats_interval(Some(Duration::from_secs(1)))
        .build(now)
}
pub fn create(rtc: &mut Rtc) -> (String, SdpPendingOffer, Mid, Mid) {
    let mut changes = rtc.sdp_api();
    let video = changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
    let audio = changes.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
    for config in channel_configs() {
        changes.add_channel_with_config(config);
    }
    let (offer, pending) = changes.apply().expect("new recvonly media produces offer");
    (offer.to_sdp_string(), pending, video, audio)
}
#[cfg(test)]
mod tests {
    use super::*;
    const OFFER_CONTRACT: &str = include_str!("../../../../fixtures/webrtc-offer-contract.json");
    #[test]
    fn offer_matches_browser_media_order_directions_codecs_and_channels() {
        let mut r = rtc(Instant::now(), true);
        let (s, _, _, _) = create(&mut r);
        let contract: serde_json::Value = serde_json::from_str(OFFER_CONTRACT).unwrap();
        let positions: Vec<usize> = contract["mediaOrder"]
            .as_array()
            .unwrap()
            .iter()
            .map(|kind| s.find(&format!("m={}", kind.as_str().unwrap())).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let recvonly = contract["directions"]
            .as_object()
            .unwrap()
            .values()
            .filter(|direction| direction.as_str() == Some("recvonly"))
            .count();
        assert_eq!(s.matches("a=recvonly").count(), recvonly);
        assert!(s.contains("H265/90000"));
        assert!(s.contains("H264/90000"));
        assert!(s.contains("opus/48000/2"));
        assert!(s.find("H265/90000").unwrap() < s.find("H264/90000").unwrap());
        for refused in ["VP8/", "VP9/", "AV1/", "PCMU/", "PCMA/"] {
            assert!(!s.contains(refused));
        }
        assert!(s.contains("packetization-mode=1"));
        assert!(s.contains("level-asymmetry-allowed=1"));
        assert!(s.contains("profile-level-id=42e01f"));
        assert!(s.contains("profile-id=1"));
        assert!(s.contains("minptime=10"));
        assert!(s.contains("useinbandfec=1"));
        let configs = channel_configs();
        let expected = contract["dataChannels"].as_array().unwrap();
        assert_eq!(configs.len(), expected.len());
        for (config, want) in configs.iter().zip(expected) {
            assert_eq!(config.label, want["label"].as_str().unwrap());
            assert_eq!(config.ordered, want["ordered"].as_bool().unwrap());
            let reliability = match want["maxRetransmits"].as_u64() {
                None => Reliability::Reliable,
                Some(retransmits) => Reliability::MaxRetransmits {
                    retransmits: retransmits as u16,
                },
            };
            assert_eq!(config.reliability, reliability);
            assert_eq!(config.negotiated, None);
        }
    }
    #[test]
    fn capability_fallback_omits_hevc_but_keeps_h264_opus_and_application() {
        let mut r = rtc(Instant::now(), false);
        let (s, _, _, _) = create(&mut r);
        assert!(!s.contains("H265/"));
        assert!(s.contains("H264/90000"));
        assert!(s.contains("opus/48000/2"));
        assert!(s.contains("m=application"));
    }
}
