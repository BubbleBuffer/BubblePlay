# Session UI (web and Android) — intent

Status: proposed 2026-10-01. Requirements are the owner's (2026-10-01); layout details are the main session's design and open to change.

## Owner requirements

- The Android app's model is the reference: one dedicated view, a permanently docked slim bar of icon quick actions, each opening a small popover that closes on an outside tap. Auto-hiding the bar was rejected (2026-10-01): losing a little screen is worth it.
- The web UI must lose its "web smell": no bulky top bar, no tabbed control sheet, no action that takes 2–3 clicks through submenus.
- Web on a desktop needs no keyboard button and no zoom buttons. It keeps a fullscreen button. Monitor switch is a quick action that opens the monitor list.
- Both web and Android get a settings view.

## Web, desktop (pointer + keyboard)

- One docked strip at the top, a single row at most 32 px high. The stream is laid out below it (never overlaid). It replaces `#top-seam`, `#desktop-toolbar` and `#control-sheet`.
- Contents, left to right: status dot with a short state label (details in a tooltip; Retry appears only on error) · Monitors · Shortcuts · Files (only with the host's `fileTransfer` capability) · "Copy remote clipboard" chip (only when the browser refused the automatic copy) · Fullscreen · Settings.
- Popovers anchor to their icon and close on outside click or Esc. One click opens, one click acts.
  - Monitors: list with the current one marked; disabled when the host has one monitor.
  - Shortcuts: chords the browser cannot pass through outside fullscreen (Ctrl+W, Ctrl+T, Ctrl+N, Ctrl+Shift+T, Alt+Tab, Alt+F4, Win, Win+D, Ctrl+Shift+Esc) plus "Capture cursor" for pointer-lock apps. No Ctrl+Alt+Del (Windows rejects injected SAS).
- Fullscreen hides the strip completely (Keyboard Lock applies). A small handle appears after the pointer rests on the top edge for 300 ms and brings the strip back as an overlay until the pointer leaves it.
- No brand wordmark in the strip.

## Web, touch (phone or tablet browser)

- Same model as the Android app: bar docked at the bottom with Keyboard, Monitors, Touch mode, Shortcuts, Keys, Arrows, More. The Android app stays the primary phone client; the web touch layout only needs to be consistent and usable.

## Settings view (web)

A slide-over panel opened from the strip, sections in this order:
- Stream: quality (Auto, High, Balanced, Data Saver).
- Input: capture keyboard shortcuts in fullscreen (Keyboard Lock), scroll speed.
- Display: default monitor for new sessions.
- Browser access: Sign out, Forget this browser.
- Diagnostics: collapsed by default; Show, Copy report.
- About: relay version, host build.

Settings persist in `localStorage` and apply without a reload.

## Settings view (Android)

Opened from More in the bar without ending the session, and from the connection screen.
- Connection: server address; Sign out.
- Touch: default touch mode, trackpad pointer speed, tap-to-click on/off.
- Display: start in full screen, keep screen on, default monitor.
- Video: codec (Auto, HEVC, H.264).
- Diagnostics: decoder name, resolution, fps, bitrate, round-trip time; Copy report.
- About: app version.

Settings persist and apply without restarting the session where possible (codec change reconnects).
