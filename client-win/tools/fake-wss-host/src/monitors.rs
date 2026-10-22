//! The scripted monitor list behind `--monitors`. The fake's display choice is
//! a list and an index, not a capture: a switch changes the index, bumps the
//! media generation and asks for a keyframe. The wire shapes mirror
//! `host/src/signaling.rs` (`monitor-list`, `monitor-error`, `desktop-state`),
//! and the two-monitor list is the one in `fixtures/follow-window-contract.json`.

use serde_json::{json, Value};

/// The longest selector the host and the browser accept.
pub const SELECTOR_MAX_BYTES: usize = 128;
/// The longest monitor list the browser accepts (`handleMonitorList`).
pub const MAX_MONITORS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub selector: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
}

/// What `--monitors` turns on. `enabled` false keeps the single-display fake
/// exactly as it was: no list is sent and the monitor controls are unhandled.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Knobs {
    pub enabled: bool,
    /// The list served; the first entry is selected at start.
    pub monitors: Vec<Monitor>,
    /// Switches to these selectors fail with a scripted `monitor-error`.
    pub failing: Vec<String>,
}

pub const SCRIPTED_ERROR_REASON: &str = "scripted monitor error";
pub const NOT_FOUND_REASON: &str = "monitor not found";

impl Knobs {
    /// The two-monitor list the fake serves with `--monitors`.
    pub fn scripted() -> Self {
        Self {
            enabled: true,
            monitors: vec![
                Monitor {
                    selector: r"\\.\DISPLAY1".into(),
                    name: "Monitor 1".into(),
                    width: 3840,
                    height: 2160,
                    origin_x: 0,
                    origin_y: 0,
                },
                Monitor {
                    selector: r"\\.\DISPLAY2".into(),
                    name: "Monitor 2".into(),
                    width: 2560,
                    height: 1440,
                    origin_x: 3840,
                    origin_y: 0,
                },
            ],
            failing: Vec::new(),
        }
    }

    /// The index of `selector` in the list, if it is there.
    pub fn position(&self, selector: &str) -> Option<usize> {
        self.monitors.iter().position(|m| m.selector == selector)
    }

    /// Whether a switch to `selector` is scripted to fail.
    pub fn fails(&self, selector: &str) -> bool {
        self.failing.iter().any(|f| f == selector)
    }
}

/// A selector as the host reads it: trimmed, non-empty, bounded. Anything else
/// is not a monitor control and is counted as unhandled.
pub fn bounded_selector(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= SELECTOR_MAX_BYTES)
}

/// `monitor-list` for the current selection. `selected` is the index in use.
pub fn monitor_list_message(monitors: &[Monitor], selected: usize) -> String {
    let entries: Vec<Value> = monitors
        .iter()
        .enumerate()
        .map(|(index, m)| {
            json!({
                "selector": m.selector,
                "name": m.name,
                "width": m.width,
                "height": m.height,
                "origin_x": m.origin_x,
                "origin_y": m.origin_y,
                "selected": index == selected,
            })
        })
        .collect();
    json!({"type": "monitor-list", "monitors": entries}).to_string()
}

pub fn monitor_error_message(selector: &str, reason: &str) -> String {
    json!({"type": "monitor-error", "selector": selector, "reason": reason}).to_string()
}

/// `desktop-state` without a desktop field, as the host sends it for a monitor
/// switch (`send_desktop_state`).
pub fn desktop_state_message(state: &str, generation: u32, reason: &str) -> String {
    json!({"type": "desktop-state", "state": state, "generation": generation, "reason": reason})
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_list_has_two_monitors_with_the_fixture_geometry() {
        let knobs = Knobs::scripted();
        assert!(knobs.enabled);
        assert_eq!(knobs.monitors.len(), 2);
        assert_eq!(knobs.monitors[0].selector, r"\\.\DISPLAY1");
        assert_eq!(knobs.monitors[1].origin_x, 3840);
        assert_eq!(knobs.position(r"\\.\DISPLAY2"), Some(1));
        assert_eq!(knobs.position(r"\\.\DISPLAY9"), None);
    }

    #[test]
    fn monitor_list_names_every_field_and_the_selection() {
        let knobs = Knobs::scripted();
        let text = monitor_list_message(&knobs.monitors, 1);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "monitor-list");
        let list = value["monitors"].as_array().unwrap();
        assert_eq!(list.len(), 2);
        for entry in list {
            for key in ["selector", "name", "width", "height", "origin_x", "origin_y", "selected"] {
                assert!(entry.get(key).is_some(), "monitor-list entry has {key}");
            }
        }
        assert_eq!(list[0]["selected"], false);
        assert_eq!(list[1]["selected"], true);
        assert_eq!(list[1]["width"], 2560);
        assert!(value.get("follow").is_none(), "follow is not advertised");
    }

    #[test]
    fn selectors_are_trimmed_and_bounded() {
        assert_eq!(bounded_selector(Some("  DISPLAY1 ")), Some("DISPLAY1"));
        assert_eq!(bounded_selector(Some("   ")), None);
        assert_eq!(bounded_selector(None), None);
        let long = "x".repeat(SELECTOR_MAX_BYTES + 1);
        assert_eq!(bounded_selector(Some(long.as_str())), None);
    }

    #[test]
    fn desktop_state_and_error_carry_the_host_fields() {
        let state: Value =
            serde_json::from_str(&desktop_state_message("unavailable", 7, "monitor-switch"))
                .unwrap();
        assert_eq!(
            state,
            json!({"type":"desktop-state","state":"unavailable","generation":7,"reason":"monitor-switch"})
        );
        let error: Value =
            serde_json::from_str(&monitor_error_message("DISPLAY9", NOT_FOUND_REASON)).unwrap();
        assert_eq!(error["type"], "monitor-error");
        assert_eq!(error["selector"], "DISPLAY9");
        assert_eq!(error["reason"], NOT_FOUND_REASON);
    }

    #[test]
    fn scripted_failures_are_matched_exactly() {
        let mut knobs = Knobs::scripted();
        knobs.failing.push(r"\\.\DISPLAY2".into());
        assert!(knobs.fails(r"\\.\DISPLAY2"));
        assert!(!knobs.fails(r"\\.\DISPLAY1"));
    }
}
