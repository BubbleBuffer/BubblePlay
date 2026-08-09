//! Pure adaptive-quality policy. Hardware and signaling consume its decisions.
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Low,
    Balanced,
    Full,
}

impl Profile {
    pub fn dimensions(self) -> (u32, u32, u32) {
        match self {
            Self::Low => (960, 540, 30),
            Self::Balanced => (1280, 720, 30),
            Self::Full => (1920, 1080, 60),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Low => "540p30",
            Self::Balanced => "720p30",
            Self::Full => "1080p60",
        }
    }
    fn range(self, cap: u32) -> (u32, u32) {
        let (minimum, maximum) = match self {
            Self::Low => (500_000, 1_400_000),
            Self::Balanced => (1_000_000, 3_500_000),
            Self::Full => (4_000_000, cap),
        };
        (minimum.min(cap), maximum.min(cap))
    }
    fn down(self) -> Self {
        match self {
            Self::Full => Self::Balanced,
            Self::Balanced => Self::Low,
            Self::Low => Self::Low,
        }
    }
    fn up(self) -> Self {
        match self {
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
}
impl Controller {
    pub fn new(cap: u32, initial: u32) -> Self {
        let profile = Profile::Balanced;
        let (min, max) = profile.range(cap);
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
        }
    }
    pub fn fixed(profile: Profile, cap: u32, target: u32) -> Self {
        let (min, max) = profile.range(cap);
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
        }
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
        let (min, max) = profile.range(self.cap);
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
        self.profile = decision.profile;
        self.target = decision.target_bps;
        self.low_since = None;
        self.healthy_since = None;
        if matches!(decision.reason, Reason::BweDown | Reason::QosDown) {
            self.upgrade_blocked_until = Some(now + Duration::from_secs(15));
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
        let below = matches!(self.profile, Profile::Balanced if raw_bwe < 1_500_000)
            || matches!(self.profile, Profile::Full if raw_bwe < 5_000_000);
        if below {
            let since = self.low_since.get_or_insert(now);
            if now.duration_since(*since) >= Duration::from_secs(1) {
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
        if self.bad_qos >= 2 {
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
            Profile::Low => 2_200_000.0,
            Profile::Balanced => 7_500_000.0,
            Profile::Full => f64::INFINITY,
        };
        if !qos_missing
            && self.upgrade_blocked_until.map_or(true, |at| now >= at)
            && (self.profile != Profile::Balanced || self.cap >= 4_000_000)
            && bwe >= threshold
        {
            let healthy_since = self.healthy_since.get_or_insert(now);
            let required = if self.profile == Profile::Low { 10 } else { 15 };
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
        let (min, max) = self.profile.range(self.cap);
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
    fn downshift_clamps_target_to_new_profile() {
        let t = Instant::now();
        let mut c = Controller::new(12_000_000, 1_500_000);
        let _ = c.bwe_sample(t, 1_000_000);
        let d = c.bwe_sample(t + Duration::from_secs(1), 1_000_000).unwrap();
        assert_eq!(d.profile, Profile::Low);
        assert!(d.target_bps <= 1_400_000);
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
        let ready = t + Duration::from_secs(15);
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
        assert!(d.target_bps >= 4_000_000);
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
