//! The end of an edge-switch window drag (see `docs/architecture/follow-active-window.md`).
//!
//! After the edge hold carried the stream to the next monitor, the host warps the cursor so the
//! dragged window lands centred on that monitor with its grab point under the cursor, then, on a
//! later tick, releases the left button. Warping first matters: a release at the old screen
//! edge would trigger Aero Snap. The user can press on the title bar again to keep moving it.
//!
//! This file is only the decisions (where, when); `main.rs` does the Win32 calls and the input.

use std::time::{Duration, Instant};

use crate::follow_window::{Rect, EDGE_ZONE_PX};

/// The button is released at the latest this long after the warp, whatever the window did.
pub const DROP_DEADLINE: Duration = Duration::from_millis(250);

/// Where the cursor goes so a window of `window` (its rectangle now, grabbed at `cursor`) ends up
/// centred on `to`, grab point preserved. A window larger than `to` is aligned to its top-left;
/// the result always lies inside `to`, clear of the snap zone. `None`: the window is unknown,
/// so the cursor goes to the centre of `to`.
pub fn drop_cursor(cursor: (i32, i32), window: Option<Rect>, to: Rect) -> (i32, i32) {
    let centre = (
        to.left + (to.right - to.left) / 2,
        to.top + (to.bottom - to.top) / 2,
    );
    let Some(w) = window else { return centre };
    let (width, height) = (w.right - w.left, w.bottom - w.top);
    let place = |origin: i32, size: i32, lo: i32, hi: i32| {
        (lo + (hi - lo - size) / 2).clamp(lo, (hi - size).max(lo)) - origin
    };
    let dx = place(w.left, width, to.left, to.right);
    let dy = place(w.top, height, to.top, to.bottom);
    let inset = EDGE_ZONE_PX * 2;
    (
        (cursor.0 + dx).clamp(to.left + inset, (to.right - 1 - inset).max(to.left + inset)),
        (cursor.1 + dy).clamp(to.top + inset, (to.bottom - 1 - inset).max(to.top + inset)),
    )
}

/// Why the button was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedBy {
    /// A tick after the warp saw the window moved, or the move loop gone.
    Tick,
    /// The window had not moved by [`DROP_DEADLINE`]: released anyway.
    Deadline,
}

impl ReleasedBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tick => "tick",
            Self::Deadline => "deadline",
        }
    }
}

/// A drop that has warped the cursor and still owes the release.
#[derive(Debug)]
pub struct PendingDrop {
    deadline: Instant,
    window_at_warp: Option<Rect>,
}

impl PendingDrop {
    /// Start a drop on the tick of the edge switch. `loop_active` is whether the move loop is
    /// still running: if it ended already, the user let go and there is nothing to do (`None`).
    /// Otherwise `warp` is called once with the cursor position to go to, and the drop is
    /// returned with that position and whether the warp worked. The release is owed from a
    /// later tick either way: a failed warp must never leave the button down.
    pub fn start(
        now: Instant,
        loop_active: bool,
        cursor: (i32, i32),
        window: Option<Rect>,
        to: Rect,
        warp: impl FnOnce((i32, i32)) -> bool,
    ) -> Option<(Self, (i32, i32), bool)> {
        loop_active.then(|| {
            let point = drop_cursor(cursor, window, to);
            let warped = warp(point);
            let drop = Self {
                deadline: now + DROP_DEADLINE,
                window_at_warp: window,
            };
            (drop, point, warped)
        })
    }

    /// One later tick: `window` is the window's rectangle now, `None` when the move loop or the
    /// window is gone. `Some` means release the left button now; it is owed until this answers.
    /// Never answers on the warp tick, because `start` does not call it.
    pub fn step(&self, now: Instant, window: Option<Rect>) -> Option<ReleasedBy> {
        if window.is_none() || window != self.window_at_warp {
            return Some(ReleasedBy::Tick);
        }
        (now >= self.deadline).then_some(ReleasedBy::Deadline)
    }
}

/// The rectangle of a window (Win32); `None` when it is gone.
pub fn window_rect(window: isize) -> Option<Rect> {
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

    let mut rect = RECT::default();
    // SAFETY: plain Win32 query with a valid out-pointer.
    unsafe { GetWindowRect(HWND(window as *mut _), &raw mut rect).ok()? };
    Some(Rect {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TO: Rect = Rect::new(1920, 0, 2560, 1440);

    #[test]
    fn the_cursor_lands_so_the_window_is_centred_with_its_grab_point() {
        // 800x600 window at (1000,300) grabbed 100 px right of and 10 px below its corner.
        let win = Rect::new(1000, 300, 800, 600);
        let (x, y) = drop_cursor((1100, 310), Some(win), TO);
        // Centred: left = 1920 + (2560-800)/2 = 2800, top = (1440-600)/2 = 420.
        assert_eq!((x, y), (2800 + 100, 420 + 10));
        // Unknown window: centre of the monitor.
        assert_eq!(drop_cursor((1100, 310), None, TO), (3200, 720));
        // A window bigger than the monitor aligns to its top-left; the cursor stays inside,
        // clear of the snap zone.
        let big = Rect::new(0, 0, 5000, 3000);
        let (x, y) = drop_cursor((4990, 5), Some(big), TO);
        assert!(x <= TO.right - 1 - 16 && y >= 16, "{x},{y}");
    }

    #[test]
    fn the_release_waits_for_a_later_tick_and_is_forced_at_the_deadline() {
        let t = Instant::now();
        let win = Rect::new(100, 100, 800, 600);
        // The move loop ended before the warp: nothing to do, nothing to release.
        assert!(PendingDrop::start(t, false, (900, 300), Some(win), TO, |_| true).is_none());
        // A failed warp still owes the release.
        let (drop, _, warped) =
            PendingDrop::start(t, true, (900, 300), Some(win), TO, |_| false).unwrap();
        assert!(!warped);
        // The window has not moved yet: wait, then force the release at the deadline.
        assert_eq!(drop.step(t + Duration::from_millis(50), Some(win)), None);
        assert_eq!(
            drop.step(t + DROP_DEADLINE, Some(win)),
            Some(ReleasedBy::Deadline)
        );
        // It moved, or the loop is gone: release on that tick.
        let moved = Rect::new(2800, 420, 800, 600);
        assert_eq!(
            drop.step(t + Duration::from_millis(50), Some(moved)),
            Some(ReleasedBy::Tick)
        );
        assert_eq!(
            drop.step(t + Duration::from_millis(50), None),
            Some(ReleasedBy::Tick)
        );
    }
}
