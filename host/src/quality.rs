//! Pure adaptive-quality policy. Hardware and signaling consume its decisions.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    Work,
    Game,
}
impl StreamMode {
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value {
            Some("work") => Some(Self::Work),
            Some("game") => Some(Self::Game),
            _ => None,
        }
    }
    pub const fn label(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Game => "game",
        }
    }
    fn profile_downshift_hold(self) -> Duration {
        Duration::from_secs(match self {
            Self::Work => 8,
            Self::Game => 3,
        })
    }
    fn bad_qos_windows(self) -> u8 {
        match self {
            Self::Work => 6,
            Self::Game => 3,
        }
    }
    fn upgrade_block(self) -> Duration {
        Duration::from_secs(match self {
            Self::Work => 30,
            Self::Game => 15,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Mobile,
    Low,
    Balanced,
    Full,
}

impl Profile {
    pub fn dimensions(self) -> (u32, u32, u32) {
        match self {
            Self::Mobile => (854, 480, 24),
            Self::Low => (960, 540, 30),
            Self::Balanced => (1280, 720, 30),
            Self::Full => (1920, 1080, 60),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Mobile => "480p24",
            Self::Low => "540p30",
            Self::Balanced => "720p30",
            Self::Full => "1080p60",
        }
    }
    fn range(self, cap: u32, mode: StreamMode) -> (u32, u32) {
        let (minimum, maximum) = match (mode, self) {
            (_, Self::Mobile) => (350_000, 1_000_000),
            (_, Self::Low) => (800_000, 2_000_000),
            (StreamMode::Work, Self::Balanced) => (1_500_000, 4_000_000),
            (StreamMode::Work, Self::Full) => (3_200_000, 8_000_000),
            (StreamMode::Game, Self::Balanced) => (3_000_000, 7_000_000),
            (StreamMode::Game, Self::Full) => (6_000_000, 12_000_000),
        };
        (minimum.min(cap), maximum.min(cap))
    }
    fn down(self) -> Self {
        match self {
            Self::Full => Self::Balanced,
            Self::Balanced => Self::Low,
            Self::Low => Self::Mobile,
            Self::Mobile => Self::Mobile,
        }
    }
    fn up(self) -> Self {
        match self {
            Self::Mobile => Self::Low,
            Self::Low => Self::Balanced,
            Self::Balanced => Self::Full,
            Self::Full => Self::Full,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Qos {
    pub loss_pct: Option<f64>,
    pub packets: Option<u64>,
    pub decode_backlog_growth: Option<i64>,
    pub freeze_delta: Option<u64>,
    pub admitted_fps: Option<f64>,
}
impl Qos {
    fn is_informative(self) -> bool {
        self.loss_pct.is_some()
            || self.packets.is_some()
            || self.decode_backlog_growth.is_some()
            || self.freeze_delta.is_some()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Startup,
    BweDown,
    HealthyUpgrade,
    QosDown,
    Bitrate,
}
impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::BweDown => "bwe-down",
            Self::HealthyUpgrade => "stable-up",
            Self::QosDown => "qos-down",
            Self::Bitrate => "bitrate",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub profile: Profile,
    pub target_bps: u32,
    pub reason: Reason,
}

pub struct Controller {
    profile: Profile,
    cap: u32,
    filtered: Option<f64>,
    target: u32,
    low_since: Option<Instant>,
    healthy_since: Option<Instant>,
    upgrade_blocked_until: Option<Instant>,
    last_up: Option<Instant>,
    bad_qos: u8,
    last_qos: Option<Instant>,
    mode: StreamMode,
}
impl Controller {
    pub fn new(cap: u32, initial: u32) -> Self {
        Self::new_with_mode(cap, initial, StreamMode::Work)
    }
    pub fn new_with_mode(cap: u32, initial: u32, mode: StreamMode) -> Self {
        Self::adaptive(Profile::Balanced, cap, initial, mode)
    }
    pub fn adaptive(profile: Profile, cap: u32, initial: u32, mode: StreamMode) -> Self {
        let (min, max) = profile.range(cap, mode);
        Self {
            profile,
            cap,
            filtered: None,
            target: initial.clamp(min, max),
            low_since: None,
            healthy_since: None,
            upgrade_blocked_until: None,
            last_up: None,
            bad_qos: 0,
            last_qos: None,
            mode,
        }
    }
    pub fn fixed(profile: Profile, cap: u32, target: u32) -> Self {
        Self::fixed_with_mode(profile, cap, target, StreamMode::Work)
    }
    pub fn fixed_with_mode(profile: Profile, cap: u32, target: u32, mode: StreamMode) -> Self {
        let (min, max) = profile.range(cap, mode);
        Self {
            profile,
            cap,
            filtered: None,
            target: target.clamp(min, max),
            low_since: None,
            healthy_since: None,
            upgrade_blocked_until: None,
            last_up: None,
            bad_qos: 0,
            last_qos: None,
            mode,
        }
    }
    pub fn mode(&self) -> StreamMode {
        self.mode
    }
    pub fn set_mode(&mut self, mode: StreamMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        let (min, max) = self.profile.range(self.cap, mode);
        self.target = self.target.clamp(min, max);
        self.low_since = None;
        self.healthy_since = None;
        self.bad_qos = 0;
    }
    pub fn profile(&self) -> Profile {
        self.profile
    }
    pub fn bwe(&self) -> Option<u32> {
        self.filtered.map(|v| v as u32)
    }
    pub fn target(&self) -> u32 {
        self.target
    }
    fn shift(&self, profile: Profile, reason: Reason) -> Decision {
        let (min, max) = profile.range(self.cap, self.mode);
        let proposed = self
            .filtered
            .map(|bwe| (bwe * 0.8) as u32)
            .unwrap_or(self.target);
        Decision {
            profile,
            target_bps: proposed.clamp(min, max),
            reason,
        }
    }
    /// Commits a decision only after its corresponding encoder reconfigure has
    /// succeeded. A failed hardware call therefore remains eligible to retry.
    pub fn apply(&mut self, now: Instant, decision: Decision) {
        let was_target = self.target;
        let profile_changed = self.profile != decision.profile;
        self.profile = decision.profile;
        self.target = decision.target_bps;
        // Bitrate updates are intentionally fast, but they must not restart a
        // pending spatial hysteresis window. Only an admitted rung change
        // establishes a new profile epoch.
        if profile_changed {
            self.low_since = None;
            self.healthy_since = None;
            self.bad_qos = 0;
        }
        if matches!(decision.reason, Reason::BweDown | Reason::QosDown) {
            self.upgrade_blocked_until = Some(now + self.mode.upgrade_block());
        }
        if decision.reason == Reason::Bitrate && decision.target_bps > was_target {
            self.last_up = Some(now);
        }
    }
    pub fn bwe_sample(&mut self, now: Instant, raw_bwe: u32) -> Option<Decision> {
        let filtered = self
            .filtered
            .map(|old| old * 0.8 + raw_bwe as f64 * 0.2)
            .unwrap_or(raw_bwe as f64);
        self.filtered = Some(filtered);
        let below = matches!(self.profile, Profile::Low if raw_bwe < 900_000)
            || matches!(self.profile, Profile::Balanced if raw_bwe < 1_500_000)
            || matches!(self.profile, Profile::Full if raw_bwe < 5_000_000);
        if below {
            let since = self.low_since.get_or_insert(now);
            if now.duration_since(*since) >= self.mode.profile_downshift_hold() {
                return Some(self.shift(self.profile.down(), Reason::BweDown));
            }
        } else {
            self.low_since = None;
        }
        self.rate_decision(now)
    }
    pub fn qos_sample(&mut self, now: Instant, qos: Qos) -> Option<Decision> {
        if !qos.is_informative() {
            return None;
        }
        self.last_qos = Some(now);
        let bad = (qos.loss_pct.unwrap_or_default() >= 5.0
            && qos.packets.unwrap_or_default() >= 100)
            || qos.decode_backlog_growth.unwrap_or_default() > 4
            || (qos.freeze_delta.unwrap_or_default() > 0
                && qos.admitted_fps.unwrap_or_default() >= 10.0);
        self.bad_qos = if bad {
            self.bad_qos.saturating_add(1)
        } else {
            0
        };
        if self.bad_qos >= self.mode.bad_qos_windows() {
            self.bad_qos = 0;
            return Some(self.shift(self.profile.down(), Reason::QosDown));
        }
        None
    }
    pub fn tick(&mut self, now: Instant) -> Option<Decision> {
        let Some(bwe) = self.filtered else {
            return None;
        };
        let qos_missing = self
            .last_qos
            .map_or(true, |at| now.duration_since(at) > Duration::from_secs(3));
        let threshold = match self.profile {
            Profile::Mobile => 1_200_000.0,
            Profile::Low => 2_200_000.0,
            Profile::Balanced => 7_500_000.0,
            Profile::Full => f64::INFINITY,
        };
        if !qos_missing
            && self.upgrade_blocked_until.map_or(true, |at| now >= at)
            && match self.profile {
                Profile::Mobile => self.cap >= 500_000,
                Profile::Low => self.cap >= 1_000_000,
                Profile::Balanced => self.cap >= 4_000_000,
                Profile::Full => false,
            }
            && bwe >= threshold
        {
            let healthy_since = self.healthy_since.get_or_insert(now);
            let required = match (self.mode, self.profile) {
                (StreamMode::Work, Profile::Mobile) => 20,
                (StreamMode::Work, Profile::Low) => 25,
                (StreamMode::Work, Profile::Balanced) => 30,
                (StreamMode::Game, Profile::Mobile) => 8,
                (StreamMode::Game, Profile::Low) => 10,
                (StreamMode::Game, Profile::Balanced) => 15,
                (_, Profile::Full) => unreachable!(),
            };
            if now.duration_since(*healthy_since) >= Duration::from_secs(required) {
                return Some(self.shift(self.profile.up(), Reason::HealthyUpgrade));
            }
        } else {
            self.healthy_since = None;
        }
        self.rate_decision(now)
    }
    fn rate_decision(&mut self, now: Instant) -> Option<Decision> {
        let bwe = self.filtered? as u32;
        let (min, max) = self.profile.range(self.cap, self.mode);
        let wanted = ((bwe as f64 * 0.8) as u32).clamp(min, max);
        let delta = self.target.abs_diff(wanted);
        if delta < 250_000 && delta * 10 < self.target {
            return None;
        }
        if wanted < self.target {
            return Some(Decision {
                profile: self.profile,
                target_bps: wanted,
                reason: Reason::Bitrate,
            });
        }
        if self
            .last_up
            .map_or(false, |at| now.duration_since(at) < Duration::from_secs(1))
        {
            return None;
        }
        let stepped = wanted.min(self.target.saturating_mul(115) / 100);
        if stepped == self.target {
            return None;
        }
        Some(Decision {
            profile: self.profile,
            target_bps: stepped,
            reason: Reason::Bitrate,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sustained_sub_megabit_bwe_enters_480p24_with_headroom() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        let _ = c.bwe_sample(t, 700_000);
        let first = c.bwe_sample(t + Duration::from_secs(8), 700_000).unwrap();
        c.apply(t + Duration::from_secs(8), first);
        let _ = c.bwe_sample(t + Duration::from_secs(8), 700_000);
        let d = c.bwe_sample(t + Duration::from_secs(16), 700_000).unwrap();
        assert_eq!(d.profile, Profile::Mobile);
        assert_eq!(d.target_bps, 560_000);
        assert_eq!(Profile::Mobile.dimensions(), (854, 480, 24));
        assert_eq!(Profile::Mobile.name(), "480p24");
    }
    #[test]
    fn downshift_clamps_target_to_new_profile() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        let _ = c.bwe_sample(t, 1_000_000);
        let d = c.bwe_sample(t + Duration::from_secs(8), 1_000_000).unwrap();
        assert_eq!(d.profile, Profile::Low);
        assert!(d.target_bps <= 2_000_000);
    }

    #[test]
    fn brief_bwe_jitter_changes_bitrate_but_not_resolution() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 3_000_000);
        for seconds in 0..8 {
            if let Some(d) = c.bwe_sample(t + Duration::from_secs(seconds), 1_000_000) {
                assert_eq!(d.profile, Profile::Balanced);
                c.apply(t + Duration::from_secs(seconds), d);
            }
        }
        let recovered = c.bwe_sample(t + Duration::from_secs(8), 6_000_000);
        assert_ne!(recovered.map(|d| d.profile), Some(Profile::Low));
        assert_eq!(c.profile(), Profile::Balanced);
    }

    #[test]
    fn sustained_bwe_dip_survives_bitrate_reconfigures_and_then_downshifts() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 3_000_000);
        for seconds in 0..8 {
            if let Some(d) = c.bwe_sample(t + Duration::from_secs(seconds), 1_000_000) {
                assert_eq!(d.profile, Profile::Balanced);
                c.apply(t + Duration::from_secs(seconds), d);
            }
        }
        let d = c.bwe_sample(t + Duration::from_secs(8), 1_000_000).unwrap();
        assert_eq!(d.profile, Profile::Low);
        assert_eq!(d.reason, Reason::BweDown);
    }

    #[test]
    fn work_qos_requires_six_consecutive_bad_receiver_windows_for_spatial_downshift() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 3_000_000);
        let bad = Qos {
            loss_pct: Some(8.0),
            packets: Some(200),
            ..Qos::default()
        };
        for seconds in 0..5 {
            assert_eq!(c.qos_sample(t + Duration::from_secs(seconds), bad), None);
        }
        let d = c.qos_sample(t + Duration::from_secs(5), bad).unwrap();
        assert_eq!(d.profile, Profile::Low);
        assert_eq!(d.reason, Reason::QosDown);
    }

    #[test]
    fn gaming_uses_three_second_bwe_and_three_window_qos_hysteresis() {
        let t = Instant::now();
        let mut bwe = Controller::new_with_mode(12_000_000, 4_500_000, StreamMode::Game);
        for seconds in 0..3 {
            if let Some(d) = bwe.bwe_sample(t + Duration::from_secs(seconds), 1_000_000) {
                assert_eq!(d.profile, Profile::Balanced);
                bwe.apply(t + Duration::from_secs(seconds), d);
            }
        }
        let down = bwe
            .bwe_sample(t + Duration::from_secs(3), 1_000_000)
            .unwrap();
        assert_eq!((down.profile, down.reason), (Profile::Low, Reason::BweDown));

        let mut qos = Controller::new_with_mode(12_000_000, 4_500_000, StreamMode::Game);
        let bad = Qos {
            loss_pct: Some(8.0),
            packets: Some(200),
            ..Qos::default()
        };
        assert!(qos.qos_sample(t, bad).is_none());
        assert!(qos.qos_sample(t + Duration::from_secs(1), bad).is_none());
        let down = qos.qos_sample(t + Duration::from_secs(2), bad).unwrap();
        assert_eq!((down.profile, down.reason), (Profile::Low, Reason::QosDown));
    }
    #[test]
    fn healthy_upgrade_enters_full_at_its_minimum() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        c.profile = Profile::Balanced;
        c.filtered = Some(9_000_000.0);
        let _ = c.qos_sample(
            t,
            Qos {
                loss_pct: Some(0.0),
                ..Qos::default()
            },
        );
        let _ = c.tick(t);
        let ready = t + Duration::from_secs(30);
        let _ = c.qos_sample(
            ready,
            Qos {
                loss_pct: Some(0.0),
                ..Qos::default()
            },
        );
        let d = c.tick(ready).unwrap();
        c.apply(ready, d);
        assert_eq!(d.profile, Profile::Full);
        assert!(d.target_bps >= 3_200_000);
    }

    #[test]
    fn cap_below_full_minimum_never_upgrades_to_full_or_panics() {
        let t = Instant::now();
        let mut c = Controller::new(3_000_000, 1_500_000);
        c.profile = Profile::Balanced;
        c.filtered = Some(9_000_000.0);
        let qos = Qos {
            loss_pct: Some(0.0),
            ..Qos::default()
        };
        let _ = c.qos_sample(t, qos);
        let _ = c.tick(t);
        let _ = c.qos_sample(t + Duration::from_secs(16), qos);
        assert_ne!(
            c.tick(t + Duration::from_secs(16)).map(|d| d.profile),
            Some(Profile::Full)
        );
        assert_eq!(c.profile(), Profile::Balanced);
    }

    #[test]
    fn all_null_qos_does_not_make_an_upgrade_healthy() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        c.profile = Profile::Balanced;
        c.filtered = Some(9_000_000.0);
        assert_eq!(c.qos_sample(t, Qos::default()), None);
        assert_ne!(
            c.tick(t + Duration::from_secs(20)).map(|d| d.profile),
            Some(Profile::Full)
        );
        assert_eq!(c.profile(), Profile::Balanced);
    }

    #[test]
    fn local_admitted_fps_alone_is_not_receiver_qos() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        c.profile = Profile::Balanced;
        c.filtered = Some(9_000_000.0);
        assert_eq!(
            c.qos_sample(
                t,
                Qos {
                    admitted_fps: Some(60.0),
                    ..Qos::default()
                }
            ),
            None
        );
        assert_ne!(
            c.tick(t + Duration::from_secs(20)).map(|d| d.profile),
            Some(Profile::Full)
        );
    }

    #[test]
    fn decision_does_not_change_effective_target_until_applied_and_can_retry() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        c.filtered = Some(4_000_000.0);
        let first = c.rate_decision(t).expect("pending bitrate decision");
        assert_eq!(c.target(), 1_500_000);
        let retry = c
            .rate_decision(t + Duration::from_secs(1))
            .expect("retry after hardware failure");
        assert_eq!(retry.target_bps, first.target_bps);
        c.apply(t, first);
        assert_eq!(c.target(), first.target_bps);
    }
}
