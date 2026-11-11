//! The state a PC's Durable Object keeps in memory while it runs: the core relay and the live
//! sockets. The sockets' own state is in their attachments and survives hibernation. (The front
//! object's in-memory state, the gate and the directory, is in front.rs.)

use std::collections::BTreeMap;

use bp_relay_core::directory::REPORT_EVERY_MS;
use bp_relay_core::relay::{ConnId, Relay as CoreRelay};
use worker::js_sys::{Date as JsDate, Function, Reflect};
use worker::wasm_bindgen::{JsCast, JsValue};
use worker::{Date, State, WebSocket, web_sys};

/// While the PC store is unhealthy, the alarm wakes this often to retry the write.
pub const STORE_RETRY_MS: u64 = 1_000;

/// What this object last told the directory about its host's presence (memory only: a wake
/// reports again).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reported {
    pub online: bool,
    pub at_ms: u64,
}

pub struct PcLive {
    pub core: CoreRelay,
    pub sockets: BTreeMap<ConnId, WebSocket>,
    pub pcs_healthy: bool,
    /// The last presence report the directory acknowledged, if any.
    pub reported: Option<Reported>,
    /// True when the directory has not yet been told the current state (a report failed).
    pub report_pending: bool,
    /// The alarm time last set, if known. The outer None means unknown, so the next call sets
    /// it. Some(None) means no alarm is set.
    pub armed: Option<Option<u64>>,
}

impl PcLive {
    /// The alarm change that matches the earliest deadline, or None when the armed alarm
    /// already matches. Some(None) means delete the alarm.
    pub fn alarm_change(&mut self, now: u64) -> Option<Option<u64>> {
        let mut next = self.core.next_deadline();
        let mut earlier = |at: u64| next = Some(next.map_or(at, |deadline| deadline.min(at)));
        if !self.pcs_healthy || self.report_pending {
            earlier(now.saturating_add(STORE_RETRY_MS));
        }
        // An online host's presence is repeated to the directory, so a lost object cannot leave it
        // online for ever.
        if let Some(Reported {
            online: true,
            at_ms,
        }) = self.reported
        {
            earlier(at_ms.saturating_add(REPORT_EVERY_MS));
        }
        if self.armed == Some(next) {
            return None;
        }
        self.armed = Some(next);
        Some(next)
    }
}

/// When the runtime last answered an auto-response ping on `ws`, in Unix ms. None when it has not.
/// The binding is not in workers-rs 0.8.7, so the call goes through Reflect.
pub fn auto_response_ms(state: &State, ws: &WebSocket) -> Option<u64> {
    let raw: &JsValue = state.as_raw().as_ref();
    let raw_ws: &web_sys::WebSocket = ws.as_ref();
    let ws_js: &JsValue = raw_ws.as_ref();
    let method: Function =
        Reflect::get(raw, &JsValue::from_str("getWebSocketAutoResponseTimestamp"))
            .ok()?
            .dyn_into()
            .ok()?;
    let value = method.call1(raw, ws_js).ok()?;
    let date: JsDate = value.dyn_into().ok()?;
    let ms = date.get_time();
    (ms.is_finite() && ms >= 0.0).then_some(ms as u64)
}

pub fn now_ms() -> u64 {
    Date::now().as_millis()
}
