//! Decides which frames are sent. Silence is sent for at most 200 ms, then
//! suppressed until sound returns. The first sent packet after the start or after
//! any suppressed span is marked as a discontinuity.

/// 200 ms of 10 ms frames.
pub const MAX_SILENT_FRAMES: u32 = 20;

/// Silence is digital zero. Any nonzero sample, however quiet, is sound.
pub fn frame_is_active(pcm: &[f32]) -> bool {
    pcm.iter().any(|s| *s != 0.0)
}

#[derive(Debug)]
pub struct SilenceGate {
    silent_run: u32,
    /// Silent frames sent before suppression starts.
    max_silent: u32,
    suppressing: bool,
    gap: bool,
}

impl SilenceGate {
    /// The stream starts with a discontinuity, so the first sent packet is marked.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::with_limit(MAX_SILENT_FRAMES)
    }

    /// Like `new`, with `max_silent` silent frames sent before suppression (200 ms of frames).
    pub fn with_limit(max_silent: u32) -> Self {
        Self {
            silent_run: 0,
            max_silent,
            suppressing: false,
            gap: true,
        }
    }

    /// Source data was lost or the device changed: the next sent packet is marked.
    pub fn mark_gap(&mut self) {
        self.gap = true;
    }

    /// `Some(discontinuity)` when the frame should be encoded and sent, `None` when it is suppressed.
    pub fn on_frame(&mut self, active: bool) -> Option<bool> {
        if active {
            self.silent_run = 0;
            if self.suppressing {
                self.suppressing = false;
                self.gap = true;
            }
        } else {
            if self.suppressing {
                return None;
            }
            self.silent_run += 1;
            if self.silent_run > self.max_silent {
                self.suppressing = true;
                return None;
            }
        }
        Some(std::mem::take(&mut self.gap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sends(gate: &mut SilenceGate, active: bool, n: usize) -> Vec<Option<bool>> {
        (0..n).map(|_| gate.on_frame(active)).collect()
    }

    #[test]
    fn active_detection_is_exact_digital_zero() {
        assert!(!frame_is_active(&[0.0; 960]));
        assert!(
            !frame_is_active(&[-0.0; 960]),
            "negative zero is still zero"
        );
        // Quiet but nonzero sound is preserved, not treated as silence.
        let mut one_quiet_sample = vec![0.0; 960];
        one_quiet_sample[777] = 1.0e-9;
        assert!(frame_is_active(&one_quiet_sample));
        let mut one_loud_sample = vec![0.0; 960];
        one_loud_sample[777] = -0.01;
        assert!(frame_is_active(&one_loud_sample));
    }

    #[test]
    fn first_packet_of_the_stream_is_a_discontinuity() {
        let mut gate = SilenceGate::new();
        assert_eq!(gate.on_frame(true), Some(true));
        assert_eq!(gate.on_frame(true), Some(false));
    }

    #[test]
    fn silence_is_sent_for_exactly_200_ms_then_suppressed() {
        let mut gate = SilenceGate::new();
        let out = sends(&mut gate, false, 30);
        // Frames 1..=20 are sent (200 ms), the first one marked as the stream start.
        assert_eq!(out[0], Some(true));
        assert!(out[1..20].iter().all(|v| *v == Some(false)));
        assert!(out[20..].iter().all(|v| v.is_none()), "{out:?}");
    }

    #[test]
    fn sound_after_suppression_is_marked_as_a_gap() {
        let mut gate = SilenceGate::new();
        sends(&mut gate, false, 25);
        assert_eq!(gate.on_frame(true), Some(true), "sound returns after a gap");
        assert_eq!(gate.on_frame(true), Some(false));
    }

    #[test]
    fn short_silence_between_sounds_is_sent_without_a_discontinuity() {
        let mut gate = SilenceGate::new();
        assert_eq!(gate.on_frame(true), Some(true));
        let quiet = sends(&mut gate, false, 20);
        assert!(quiet.iter().all(|v| *v == Some(false)), "{quiet:?}");
        assert_eq!(gate.on_frame(true), Some(false));
    }

    #[test]
    fn silence_counter_resets_when_sound_returns_before_the_limit() {
        let mut gate = SilenceGate::new();
        gate.on_frame(true);
        sends(&mut gate, false, 15);
        gate.on_frame(true);
        // A fresh 200 ms budget starts after the sound.
        let quiet = sends(&mut gate, false, 20);
        assert!(quiet.iter().all(|v| v.is_some()), "{quiet:?}");
        assert_eq!(gate.on_frame(false), None);
    }

    #[test]
    fn gap_marker_applies_to_the_next_sent_packet() {
        let mut gate = SilenceGate::new();
        gate.on_frame(true);
        gate.on_frame(true);
        gate.mark_gap();
        assert_eq!(gate.on_frame(true), Some(true));
        assert_eq!(gate.on_frame(true), Some(false));
    }

    #[test]
    fn long_suppression_stays_suppressed_until_sound() {
        let mut gate = SilenceGate::new();
        sends(&mut gate, false, 20);
        let later = sends(&mut gate, false, 10_000);
        assert!(later.iter().all(|v| v.is_none()));
        assert_eq!(gate.on_frame(true), Some(true));
    }
}
