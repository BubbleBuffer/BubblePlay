//! Bounded, content-free input diagnostics. Counters contain packet categories,
//! gate reasons and Windows receipt counts, never keys, text or positions.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputStatusReason {
    AdminRequired,
    AdminUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputRoute {
    Local,
    Admin,
    Secure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputRoutingStatus {
    pub reason: Option<InputStatusReason>,
    pub route: InputRoute,
}

impl InputRoutingStatus {
    pub fn normal(requires_admin: bool, enabled: bool, agent_unavailable: bool) -> Self {
        Self {
            reason: if requires_admin && !enabled {
                Some(InputStatusReason::AdminRequired)
            } else if requires_admin && agent_unavailable {
                Some(InputStatusReason::AdminUnavailable)
            } else {
                None
            },
            route: if enabled && (requires_admin || !agent_unavailable) {
                InputRoute::Admin
            } else {
                InputRoute::Local
            },
        }
    }
}

#[derive(Default)]
pub struct InputStatusChanges(Option<InputRoutingStatus>);

impl InputStatusChanges {
    pub fn observe(&mut self, status: InputRoutingStatus) -> Option<InputRoutingStatus> {
        if self.0 == Some(status) {
            return None;
        }
        self.0 = Some(status);
        Some(status)
    }

    pub fn reset(&mut self) {
        self.0 = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum InputDropReason {
    NotReady,
    SecurePolicy,
    NonDefaultDesktop,
    AdminRequired,
    AdminUnavailable,
    MappingInvalid,
    Malformed,
    StaleSequence,
    NativeDrag,
    ClipboardDisabled,
    ClipboardRejected,
    InjectionFailed,
    SecureHelperUnavailable,
}

impl InputDropReason {
    pub const ALL: [Self; 13] = [
        Self::NotReady,
        Self::SecurePolicy,
        Self::NonDefaultDesktop,
        Self::AdminRequired,
        Self::AdminUnavailable,
        Self::MappingInvalid,
        Self::Malformed,
        Self::StaleSequence,
        Self::NativeDrag,
        Self::ClipboardDisabled,
        Self::ClipboardRejected,
        Self::InjectionFailed,
        Self::SecureHelperUnavailable,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::NotReady => "not-ready",
            Self::SecurePolicy => "secure-policy",
            Self::NonDefaultDesktop => "non-default-desktop",
            Self::AdminRequired => "admin-required",
            Self::AdminUnavailable => "admin-unavailable",
            Self::MappingInvalid => "mapping-invalid",
            Self::Malformed => "malformed",
            Self::StaleSequence => "stale-sequence",
            Self::NativeDrag => "native-drag",
            Self::ClipboardDisabled => "clipboard-disabled",
            Self::ClipboardRejected => "clipboard-rejected",
            Self::InjectionFailed => "injection-failed",
            Self::SecureHelperUnavailable => "secure-helper-unavailable",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct InputDiagnosticCounters {
    /// Kinds 0..=13 in wire order; slot 14 contains unknown/empty packets.
    pub received_by_kind: [u64; 15],
    pub drops_by_reason: [u64; 13],
    /// local, admin, secure submission calls respectively.
    pub route_calls: [u64; 3],
    pub requested_events: u64,
    pub applied_events: u64,
    pub short_calls: u64,
    pub error_calls: u64,
    pub coalesced_packets: u64,
}

impl InputDiagnosticCounters {
    pub fn received(&mut self, kind: Option<u8>) {
        let index = kind.filter(|kind| *kind <= 13).map_or(14, usize::from);
        self.received_by_kind[index] = self.received_by_kind[index].saturating_add(1);
    }

    pub fn dropped(&mut self, reason: InputDropReason) {
        let count = &mut self.drops_by_reason[reason as usize];
        *count = count.saturating_add(1);
    }

    pub fn route(&mut self, route: InputRoute) {
        let index = match route {
            InputRoute::Local => 0,
            InputRoute::Admin => 1,
            InputRoute::Secure => 2,
        };
        self.route_calls[index] = self.route_calls[index].saturating_add(1);
    }

    pub fn receipt(&mut self, requested: u32, applied: u32, error: i32) {
        self.requested_events = self.requested_events.saturating_add(u64::from(requested));
        self.applied_events = self
            .applied_events
            .saturating_add(u64::from(applied.min(requested)));
        if applied != requested {
            self.short_calls = self.short_calls.saturating_add(1);
            if error != 0 {
                self.error_calls = self.error_calls.saturating_add(1);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.received_by_kind.iter().all(|count| *count == 0)
            && self.drops_by_reason.iter().all(|count| *count == 0)
            && self.route_calls.iter().all(|count| *count == 0)
            && self.coalesced_packets == 0
    }

    pub fn named_receipts(&self) -> Vec<(&'static str, u64)> {
        const KINDS: [&str; 15] = [
            "key-down",
            "key-up",
            "mouse-relative",
            "mouse-absolute",
            "buttons",
            "wheel",
            "release-all",
            "motion-v2",
            "absolute-v2",
            "buttons-at",
            "cursor-sync",
            "text",
            "clipboard",
            "scan-key",
            "unknown",
        ];
        KINDS
            .into_iter()
            .zip(self.received_by_kind)
            .filter(|(_, count)| *count > 0)
            .collect()
    }

    pub fn named_drops(&self) -> Vec<(&'static str, u64)> {
        InputDropReason::ALL
            .into_iter()
            .map(|reason| (reason.name(), self.drops_by_reason[reason as usize]))
            .filter(|(_, count)| *count > 0)
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct InputFlowTransitions {
    dropping: Option<InputDropReason>,
}

impl InputFlowTransitions {
    pub fn reason(&self) -> Option<InputDropReason> {
        self.dropping
    }

    /// `None` means flowing; an outer `None` means no transition.
    pub fn observe(&mut self, reason: Option<InputDropReason>) -> Option<Option<InputDropReason>> {
        if self.dropping == reason {
            return None;
        }
        self.dropping = reason;
        Some(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privilege_warning_and_route_follow_policy_and_agent_state() {
        let ordinary = InputRoutingStatus::normal(false, false, true);
        assert_eq!(ordinary.reason, None);
        assert_eq!(ordinary.route, InputRoute::Local);
        assert_eq!(
            InputRoutingStatus::normal(true, false, true).reason,
            Some(InputStatusReason::AdminRequired)
        );
        assert_eq!(
            InputRoutingStatus::normal(true, true, true).reason,
            Some(InputStatusReason::AdminUnavailable)
        );
        assert_eq!(
            InputRoutingStatus::normal(true, true, true).route,
            InputRoute::Admin
        );
        let available = InputRoutingStatus::normal(true, true, false);
        assert_eq!(available.reason, None);
        assert_eq!(available.route, InputRoute::Admin);
    }

    #[test]
    fn warning_emits_on_change_and_clears_once() {
        let mut changes = InputStatusChanges::default();
        let warning = InputRoutingStatus::normal(true, false, true);
        assert_eq!(changes.observe(warning), Some(warning));
        assert_eq!(changes.observe(warning), None);
        let clear = InputRoutingStatus::normal(false, false, true);
        assert_eq!(changes.observe(clear), Some(clear));
        assert_eq!(changes.observe(clear), None);
        changes.reset();
        assert_eq!(changes.observe(clear), Some(clear));
    }

    #[test]
    fn counters_preserve_receipt_kind_and_distinct_drop_reasons() {
        let mut counters = InputDiagnosticCounters::default();
        counters.received(Some(0));
        counters.received(Some(11));
        counters.received(Some(255));
        counters.received(None);
        counters.dropped(InputDropReason::NonDefaultDesktop);
        counters.dropped(InputDropReason::NonDefaultDesktop);
        counters.dropped(InputDropReason::AdminUnavailable);
        counters.route(InputRoute::Admin);
        counters.receipt(4, 2, 5);
        assert_eq!(counters.received_by_kind[0], 1);
        assert_eq!(counters.received_by_kind[11], 1);
        assert_eq!(counters.received_by_kind[14], 2);
        assert_eq!(
            counters.drops_by_reason[InputDropReason::NonDefaultDesktop as usize],
            2
        );
        assert_eq!(
            counters.drops_by_reason[InputDropReason::AdminUnavailable as usize],
            1
        );
        assert_eq!(counters.requested_events, 4);
        assert_eq!(counters.applied_events, 2);
        assert_eq!(counters.short_calls, 1);
        assert_eq!(counters.error_calls, 1);
        assert_eq!(counters.route_calls, [0, 1, 0]);
    }

    #[test]
    fn flow_transitions_report_drop_and_recovery_once() {
        let mut flow = InputFlowTransitions::default();
        assert_eq!(flow.observe(None), None);
        assert_eq!(
            flow.observe(Some(InputDropReason::NotReady)),
            Some(Some(InputDropReason::NotReady))
        );
        assert_eq!(flow.observe(Some(InputDropReason::NotReady)), None);
        assert_eq!(flow.observe(None), Some(None));
        assert_eq!(flow.observe(None), None);
    }
}
