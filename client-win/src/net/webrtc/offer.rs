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
    #[test]
    fn offer_matches_browser_media_order_directions_codecs_and_channels() {
        let mut r = rtc(Instant::now(), true);
        let (s, _, _, _) = create(&mut r);
        let video = s.find("m=video").unwrap();
        let audio = s.find("m=audio").unwrap();
        let app = s.find("m=application").unwrap();
        assert!(video < audio && audio < app);
        assert_eq!(s.matches("a=recvonly").count(), 2);
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
        let source = include_str!("../../../../worker/public/receiver.js");
        assert!(source.contains("createDataChannel(\"input-control\", { ordered: true })"));
        assert!(source.contains(
            "createDataChannel(\"input-motion\", { ordered: false, maxRetransmits: 0 })"
        ));
        let [c, m] = channel_configs();
        assert_eq!(c.label, "input-control");
        assert_eq!(m.label, "input-motion");
        assert!(c.ordered);
        assert!(!m.ordered);
        assert_eq!(c.reliability, Reliability::Reliable);
        assert_eq!(
            m.reliability,
            Reliability::MaxRetransmits { retransmits: 0 }
        );
        assert_eq!(c.negotiated, None);
        assert_eq!(m.negotiated, None);
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
