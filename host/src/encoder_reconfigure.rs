//! Coalesce encoder settings until the next encode boundary.
//!
//! The caller holds the device gate while flushing and encoding. Setters only
//! update desired state; a failed native application leaves that state pending.

use crate::encode::MAX_HEVC_QP;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_qp: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl EncoderSettings {
    fn normalized(mut self) -> Self {
        self.max_qp = normalize_max_qp(self.max_qp);
        self
    }
}

fn normalize_max_qp(max_qp: Option<u32>) -> Option<u32> {
    max_qp.filter(|qp| *qp < MAX_HEVC_QP)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconfigurePlan {
    pub settings: EncoderSettings,
    pub max_qp_changed: bool,
    pub profile_changed: bool,
    /// Profile changes and explicit rate-control resets both reset the encoder.
    pub reset_encoder: bool,
    /// Preserve the explicit reset's native forceIDR flag. Profile transitions
    /// instead force the next submitted picture, as in the existing encoder.
    pub force_idr: bool,
}

impl ReconfigurePlan {
    pub fn forces_next_idr(self) -> bool {
        self.profile_changed || self.force_idr
    }
}

pub(crate) struct PendingReconfigure {
    applied: EncoderSettings,
    desired: EncoderSettings,
    reset_requested: bool,
}

impl PendingReconfigure {
    pub fn new(settings: EncoderSettings) -> Self {
        let settings = settings.normalized();
        Self {
            applied: settings,
            desired: settings,
            reset_requested: false,
        }
    }

    pub fn applied(&self) -> EncoderSettings {
        self.applied
    }

    pub fn desired(&self) -> EncoderSettings {
        self.desired
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) {
        // Assign even when equal to applied: this cancels an older pending value.
        self.desired.bitrate_bps = bitrate_bps;
    }

    pub fn set_max_qp(&mut self, max_qp: Option<u32>) {
        self.desired.max_qp = normalize_max_qp(max_qp);
    }

    /// The encoder validates dimensions against its native maximum first.
    pub fn set_profile(&mut self, width: u32, height: u32, fps: u32) {
        self.desired.width = width;
        self.desired.height = height;
        self.desired.fps = fps;
    }

    pub fn reset_rate_control(&mut self) {
        self.reset_requested = true;
    }

    /// Invoke at most one native application before one encode attempt.
    ///
    /// The callback returns any native configuration that the caller needs to
    /// retain. Commit desired state and consume the reset only after success;
    /// return failures directly so the caller does not register/encode a frame.
    pub fn flush_with<T, E>(
        &mut self,
        apply: impl FnOnce(ReconfigurePlan) -> Result<T, E>,
    ) -> Result<Option<T>, E> {
        if self.desired == self.applied && !self.reset_requested {
            return Ok(None);
        }
        let profile_changed = self.desired.width != self.applied.width
            || self.desired.height != self.applied.height
            || self.desired.fps != self.applied.fps;
        let plan = ReconfigurePlan {
            settings: self.desired,
            max_qp_changed: self.desired.max_qp != self.applied.max_qp,
            profile_changed,
            reset_encoder: profile_changed || self.reset_requested,
            force_idr: self.reset_requested,
        };
        let retained = apply(plan)?;
        self.applied = plan.settings;
        self.reset_requested = false;
        Ok(Some(retained))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Reconfigure(usize, ReconfigurePlan),
        Encode(usize, EncoderSettings),
    }

    fn settings() -> EncoderSettings {
        EncoderSettings {
            bitrate_bps: 6_000_000,
            max_qp: None,
            width: 1920,
            height: 1080,
            fps: 60,
        }
    }

    fn encode_frame(
        pending: &mut PendingReconfigure,
        events: &mut Vec<Event>,
        frame: usize,
        fail_reconfigure: bool,
    ) -> Result<(), &'static str> {
        pending.flush_with(|plan| {
            events.push(Event::Reconfigure(frame, plan));
            if fail_reconfigure {
                Err("native reconfigure failed")
            } else {
                Ok(())
            }
        })?;
        events.push(Event::Encode(frame, pending.applied()));
        Ok(())
    }

    fn refine_frame(pending: &mut PendingReconfigure, events: &mut Vec<Event>, frame: usize) {
        let base = pending.desired().bitrate_bps;
        pending.set_bitrate(base * 4);
        encode_frame(pending, events, frame, false).unwrap();
        pending.set_bitrate(base);
    }

    fn reconfigure_count(events: &[Event], frame: usize) -> usize {
        events
            .iter()
            .filter(|event| matches!(event, Event::Reconfigure(at, _) if *at == frame))
            .count()
    }

    #[test]
    fn consecutive_refines_coalesce_restores_and_new_rate_decisions_win() {
        let mut pending = PendingReconfigure::new(settings());
        let mut events = Vec::new();
        refine_frame(&mut pending, &mut events, 0);
        assert_eq!(pending.desired().bitrate_bps, 6_000_000);
        assert_eq!(pending.applied().bitrate_bps, 24_000_000);
        refine_frame(&mut pending, &mut events, 1);
        // A newer rate decision supersedes the queued restoration.
        pending.set_bitrate(5_000_000);
        pending.set_bitrate(4_000_000);
        refine_frame(&mut pending, &mut events, 2);
        encode_frame(&mut pending, &mut events, 3, false).unwrap();

        let reconfigures: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Reconfigure(frame, plan) => Some((*frame, plan.settings.bitrate_bps)),
                _ => None,
            })
            .collect();
        assert_eq!(
            reconfigures,
            [(0, 24_000_000), (2, 16_000_000), (3, 4_000_000)]
        );
        let encoded: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Encode(_, settings) => Some(settings.bitrate_bps),
                _ => None,
            })
            .collect();
        assert_eq!(encoded, [24_000_000, 24_000_000, 16_000_000, 4_000_000]);
        for frame in 0..4 {
            assert!(reconfigure_count(&events, frame) <= 1);
        }
        assert_eq!(pending.desired(), pending.applied());
    }

    #[test]
    fn observer_detects_legacy_immediate_refine_and_restore_reconfigures() {
        let mut pending = PendingReconfigure::new(settings());
        let mut events = Vec::new();
        let base = pending.desired().bitrate_bps;
        pending.set_bitrate(base * 4);
        encode_frame(&mut pending, &mut events, 0, false).unwrap();
        // Negative control: model the old setter's immediate native application
        // when restoring after encode, instead of leaving it queued.
        pending.set_bitrate(base);
        pending
            .flush_with::<(), ()>(|plan| {
                events.push(Event::Reconfigure(0, plan));
                Ok(())
            })
            .unwrap();
        assert!(reconfigure_count(&events, 0) > 1);
        assert!(matches!(events[0], Event::Reconfigure(0, _)));
        assert!(matches!(events[1], Event::Encode(0, _)));
        assert!(matches!(events[2], Event::Reconfigure(0, _)));
    }

    #[test]
    fn returning_to_applied_settings_cancels_pending_changes() {
        let mut pending = PendingReconfigure::new(settings());
        pending.set_bitrate(24_000_000);
        pending.set_bitrate(6_000_000);
        pending.set_max_qp(Some(30));
        pending.set_max_qp(Some(MAX_HEVC_QP + 1));
        pending.set_profile(1280, 720, 30);
        pending.set_profile(1920, 1080, 60);
        let result = pending
            .flush_with::<(), ()>(|_| panic!("cancelled settings must not call the driver"))
            .unwrap();
        assert_eq!(result, None);
        assert_eq!(pending.desired().max_qp, None);
    }

    #[test]
    fn bitrate_qp_profile_and_reset_merge_into_one_application() {
        let mut pending = PendingReconfigure::new(settings());
        let mut events = Vec::new();
        pending.set_bitrate(9_000_000);
        pending.set_max_qp(Some(28));
        pending.set_profile(1280, 720, 30);
        pending.reset_rate_control();
        pending.reset_rate_control();
        encode_frame(&mut pending, &mut events, 0, false).unwrap();
        assert_eq!(events.len(), 2);
        let Event::Reconfigure(0, plan) = events[0] else {
            panic!("reconfiguration must precede encoding");
        };
        assert_eq!(plan.settings, pending.desired());
        assert_eq!(plan.settings.bitrate_bps, 9_000_000);
        assert_eq!(plan.settings.max_qp, Some(28));
        assert_eq!(
            (plan.settings.width, plan.settings.height, plan.settings.fps),
            (1280, 720, 30)
        );
        assert!(
            plan.max_qp_changed && plan.profile_changed && plan.reset_encoder && plan.force_idr
        );
        assert!(plan.forces_next_idr());
        encode_frame(&mut pending, &mut events, 1, false).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn failure_preserves_pending_settings_and_reset_until_next_frame_retry() {
        let initial = settings();
        let mut pending = PendingReconfigure::new(initial);
        let mut events = Vec::new();
        pending.set_bitrate(9_000_000);
        pending.set_max_qp(Some(28));
        pending.set_profile(1280, 720, 30);
        pending.reset_rate_control();
        let desired = pending.desired();
        assert!(encode_frame(&mut pending, &mut events, 0, true).is_err());
        assert_eq!(pending.applied(), initial);
        assert_eq!(pending.desired(), desired);
        assert_eq!(
            events.len(),
            1,
            "failed application must not encode or retry"
        );
        encode_frame(&mut pending, &mut events, 1, false).unwrap();
        assert_eq!(pending.applied(), desired);
        let (Event::Reconfigure(0, failed), Event::Reconfigure(1, retried)) =
            (&events[0], &events[1])
        else {
            panic!("one application attempt at each frame boundary");
        };
        assert_eq!(failed, retried);
        encode_frame(&mut pending, &mut events, 2, false).unwrap();
        assert_eq!(events.len(), 4, "successful retry must consume reset");
    }

    #[test]
    fn profile_changes_force_next_picture_without_explicit_native_force_idr() {
        let mut pending = PendingReconfigure::new(settings());
        pending.set_profile(1280, 720, 30);
        let retained = pending
            .flush_with::<_, ()>(|plan| {
                assert!(plan.reset_encoder && plan.profile_changed);
                assert!(!plan.force_idr);
                assert!(plan.forces_next_idr());
                Ok("native configuration")
            })
            .unwrap();
        assert_eq!(retained, Some("native configuration"));
    }

    #[test]
    fn explicit_reset_is_applied_once_even_when_settings_are_unchanged() {
        let mut pending = PendingReconfigure::new(settings());
        pending.reset_rate_control();
        pending
            .flush_with::<(), ()>(|plan| {
                assert!(plan.reset_encoder && plan.force_idr);
                assert!(!plan.profile_changed && !plan.max_qp_changed);
                Ok(())
            })
            .unwrap();
        pending
            .flush_with::<(), ()>(|_| panic!("reset must be consumed after success"))
            .unwrap();
    }

    #[test]
    fn lifting_an_applied_qp_ceiling_preserves_a_pending_native_update() {
        let mut initial = settings();
        initial.max_qp = Some(28);
        let mut pending = PendingReconfigure::new(initial);
        pending.set_max_qp(Some(MAX_HEVC_QP));
        assert_eq!(pending.applied().max_qp, Some(28));
        assert_eq!(pending.desired().max_qp, None);
        pending
            .flush_with::<(), ()>(|plan| {
                assert!(plan.max_qp_changed);
                assert_eq!(plan.settings.max_qp, None);
                assert!(!plan.reset_encoder && !plan.forces_next_idr());
                Ok(())
            })
            .unwrap();
        assert_eq!(pending.applied().max_qp, None);
    }
}
