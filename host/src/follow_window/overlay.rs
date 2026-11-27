//! Screen-wide overlay rules of "follow active window": which foreground windows count as an
//! overlay covering the screens rather than a window living on one monitor. Pure geometry over
//! monitor rectangles; see [`super`].

use super::Rect;

/// Executables whose windows the rules ignore: the Snipping Tool's capture and editor windows
/// (not shell classes, and not always wide enough to count as a screen-wide overlay), compared
/// case-insensitively.
const IGNORED_EXES: [&str; 2] = ["SnippingTool.exe", "ScreenClippingHost.exe"];

/// Whether a window of the executable `exe` is ignored by following.
pub fn is_ignored_exe(exe: &str) -> bool {
    IGNORED_EXES.iter().any(|e| e.eq_ignore_ascii_case(exe))
}

/// A window covers a monitor when it covers at least this percentage of the monitor's area.
const COVER_PERCENT: i64 = 90;

fn rect_area(r: Rect) -> i64 {
    i64::from((r.right - r.left).max(0)) * i64::from((r.bottom - r.top).max(0))
}

/// Whether `window` covers (nearly) all of the monitor rectangle `monitor`. A maximised window
/// covers the work area, which is smaller than the full monitor rectangle (the taskbar), so it
/// does not count at this threshold on a normal taskbar.
fn covers_monitor(window: Rect, monitor: Rect) -> bool {
    let overlap = Rect {
        left: window.left.max(monitor.left),
        top: window.top.max(monitor.top),
        right: window.right.min(monitor.right),
        bottom: window.bottom.min(monitor.bottom),
    };
    let monitor_area = rect_area(monitor);
    monitor_area > 0 && rect_area(overlap) * 100 >= monitor_area * COVER_PERCENT
}

/// A window that covers (nearly) all of at least two monitors is a screen-wide overlay, such as
/// the Snipping Tool's capture layer, not a window living on one monitor. Following it would jump
/// monitors, so it is ignored like a shell window.
pub fn spans_multiple_monitors(window: Rect, monitors: &[Rect]) -> bool {
    monitors
        .iter()
        .filter(|m| covers_monitor(window, **m))
        .count()
        >= 2
}

/// Whether `window` contains the whole monitor rectangle. A maximised window stops at the
/// taskbar, so it never does on a monitor with a visible taskbar; a capture overlay does.
fn fills_monitor(window: Rect, monitor: Rect) -> bool {
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
}

/// Whether following ignores a window of the executable `exe` at `window`. The Snipping Tool's
/// windows are ignored only when overlay-shaped (containing the full rectangle of a monitor, as its
/// capture layer does on each display; a 90% cover would also catch a maximised window, which
/// lacks only the taskbar); its normal and maximised app window is followed like any
/// other. Other executables are never ignored here.
pub fn is_ignored_snipping_overlay(exe: &str, window: Rect, monitors: &[Rect]) -> bool {
    is_ignored_exe(exe) && monitors.iter().any(|m| fills_monitor(window, *m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spanning_overlay_is_not_a_window_on_one_monitor() {
        let left = Rect::new(0, 0, 1920, 1080);
        let right = Rect::new(1920, 0, 2560, 1440);
        let monitors = [left, right];
        // A full virtual-screen overlay.
        assert!(spans_multiple_monitors(
            Rect::new(0, 0, 4480, 1440),
            &monitors
        ));
        // Normal windows, maximised or straddling an edge, are not.
        assert!(!spans_multiple_monitors(
            Rect::new(1920, 0, 2560, 1440),
            &monitors
        ));
        assert!(!spans_multiple_monitors(
            Rect::new(1500, 100, 1000, 800),
            &monitors
        ));
        assert!(!spans_multiple_monitors(
            Rect::new(100, 100, 800, 600),
            &monitors
        ));
        assert!(!spans_multiple_monitors(
            Rect::new(0, 0, 4480, 1440),
            &[left]
        ));
    }

    #[test]
    fn snipping_tool_is_ignored_only_as_a_capture_overlay() {
        let left = Rect::new(0, 0, 1920, 1080);
        let right = Rect::new(1920, 0, 2560, 1440);
        let monitors = [left, right];
        // (exe, window, ignored)
        let cases = [
            ("SnippingTool.exe", left, true),
            ("snippingtool.EXE", right, true),
            ("ScreenClippingHost.exe", left, true),
            // The normal window, and a maximised one (work area: the taskbar is excluded).
            ("SnippingTool.exe", Rect::new(300, 200, 900, 600), false),
            ("SnippingTool.exe", Rect::new(0, 0, 1920, 1040), false),
            (
                "ScreenClippingHost.exe",
                Rect::new(1920, 0, 2560, 1400),
                false,
            ),
            // Other executables are never ignored by name, fullscreen or not.
            ("Game.exe", left, false),
            ("", left, false),
            ("SnippingTool.exe.bak", left, false),
        ];
        for (exe, window, ignored) in cases {
            assert_eq!(
                is_ignored_snipping_overlay(exe, window, &monitors),
                ignored,
                "{exe} {window:?}"
            );
        }
        // The window list still hides every Snipping Tool window.
        assert!(is_ignored_exe("SnippingTool.exe") && !is_ignored_exe("explorer.exe"));
    }
}
