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

## Current state (web, 2026-10-01)

Built in `worker/public/index.html`, `bubbleplay.css`, `receiver.js`, plus `chrome.js` (popover controller, shortcut chords, edge reveal) and `settings.js` (settings record). Tests: `worker/test/session-chrome.test.mjs`, `cdp-boundary.test.mjs`.

- Layout follows the primary pointer: `(pointer: coarse)` gets the bottom bar (49 px including its seam, 48 px items), otherwise the 32 px top strip. `#visual`, `#stage` and `#input-surface` sit between `--strip-top` and `--strip-bottom`, so the stream is never under the strip.
- Desktop strip, left to right: status dot and short label (detail, round trip and monitor in the tooltip; Retry and a red label only on error; a "Secure desktop" chip when the host is on it) · Monitors · Shortcuts · Files · Copy remote clipboard chip · zoom reset chip · Fullscreen · Settings. No brand wordmark, no keyboard or zoom buttons.
- Touch bar: status dot · Keyboard · Monitors · Touch mode (Work/Game) · Shortcuts · Keys · Arrows · Files · More (Fullscreen, Settings). On error the status label and Retry float above the bar.
- Popovers: one open at a time; a transparent scrim takes outside presses; Esc closes without reaching the remote PC; ArrowUp/Down move between rows. A popover opened with the mouse leaves focus on `#input-surface`; one opened from the keyboard moves focus into it and returns it to its icon on close. Strip and popover buttons do not take focus on mouse press.
- Monitors: radio rows with the current one checked; the icon is disabled with one monitor. Shortcuts: the spec's chords as physical keys (touch adds Copy, Paste, Cut, Undo, Select all) and, on desktop only, "Capture cursor", which switches to Game mode and locks the pointer in one click.
- Keys (touch): latching Ctrl/Alt/Shift/Win (first letter shown on the icon), Clear, navigation keys, F1–F12. Arrows (touch): 3×3 pad; held arrows and Page keys repeat after 400 ms every 60 ms.
- Files: shown only with the host's `fileTransfer` capability. The popover holds file-ui.js's offer chip and transfer rows plus "Choose files…" (multiple-file picker, uploads to save). An offer or a running transfer puts a dot on the icon; popovers never open by themselves. Drop and paste uploads are unchanged.
- Fullscreen (desktop): the strip slides away and the stream uses the whole screen. A handle appears after the pointer rests within 4 px of the top for 300 ms; hovering or clicking it brings the strip back as an overlay, which leaves 300 ms after the pointer does unless a popover is open. Touch fullscreen keeps the bar.
- Settings slide-over (right side; full width on touch): Stream quality; Input keyboard lock (disabled where the browser has no Keyboard Lock), scroll speed (0.5×–2×), pointer mode (Work/Game); Display default monitor (requested once per page load); Browser access; Diagnostics (collapsed; Show, Copy report); About (relay version from `workerVersionId` when the relay sends it, receiver build; host build shows "Not reported" because nothing on the wire carries it).
- Settings live in `localStorage["bubbleplay.settings"]` as `{version: 1, quality, pointerMode, keyboardLock, scrollSpeed, defaultMonitor}`. A first load migrates `bp_quality` and `bp_pointer_mode`; a corrupt or other-version record falls back to defaults; storage errors are ignored.
- Zoom: desktop zoom buttons are gone; Ctrl+wheel and touch pinch still zoom, and a "1.5× · Reset" chip appears in the strip while zoomed.
- `?diag=1` expands the diagnostics report inside Settings; it no longer opens a panel over the stream.
- Lab check (2026-10-01, loopback relay and candidate host, headless Chromium): 1440×900, 1024×640 and 390×844 touch screenshots; strip 32 px at top, touch bar 49 px at bottom with 45×48 px items (eight items at 390 px); every popover opened and closed; monitor switch DISPLAY1 → DISPLAY2 and back; fullscreen handle and overlay strip worked.
