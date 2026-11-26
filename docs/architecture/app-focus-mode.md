# App focus mode

Status: approved 2026-11-21; nothing is built. Roadmap entry: [App focus mode](../roadmap.md#app-focus-mode).

App focus mode shows one app from the PC instead of the whole desktop. The user picks the app from a list, and the client crops today's single video stream down to that app's window. It needs no change to media, input packets or the relay.

This replaces the wider multi-window concept approved on 2026-11-16. That concept was cut back on 2026-11-21: simultaneous multi-monitor windows are [deferred](#deferred-simultaneous-monitors), and several apps at once are [rejected](#rejected-several-apps-at-once).

Out of scope: several apps on screen at once, resizing apps on the PC, launching apps from the client, drag and drop between client windows, and macOS or Linux hosts. Related: [follow active window](follow-active-window.md), [session UI](session-ui.md), [secure desktop](secure-desktop.md).

## Decisions (2026-11-21)

| Decision | Choice |
| --- | --- |
| How the app is shown | A client-side crop of the existing stream. No second stream, no per-window capture, no virtual display. |
| Size | The client scales the crop to fit its window. The host never resizes or moves the app. |
| Switcher | One popover listing each app's title and icon. No thumbnails, no app strip, no Alt+Tab interception, no shortcut chord. |
| Host quality | Unchanged. Favouring the app's area in the encoder is deferred. |
| Clients | Browser first, then the Windows app, then Android. |

## What exists already

- **One stream per session.** The host streams one monitor; a monitor switch happens in place on the same stream (`switch_capture_monitor!` in `host/src/main.rs`).
- **Monitor geometry.** `monitor-list` gives each monitor's selector, size and origin in virtual-desktop pixels (`MonitorInfo` in `host/src/capture.rs`).
- **Window filtering.** Follow active window already samples the foreground window every 250 ms and skips invisible, minimized, cloaked, shell and ghost windows (`host/src/follow_window.rs`).
- **Input.** Absolute positions are relative to the streamed monitor. The client already maps its pointer through the displayed picture (`videoContentRect` in `worker/public/receiver.js`).

## Behaviour

- **Entering.** "Show apps" in the session toolbar opens the switcher. Picking an app sends `window-focus`. The host restores the window if it is minimized and brings it to the foreground. If the window is on another monitor, the host switches the stream there through the existing monitor-switch path.
- **The picture.** The client crops the stream to the app's window rectangle, widened to include the visible windows the app owns (menus, dialogs, tooltips), and scales it to fit. Margins are letterboxed.
- **Input.** The client maps its pointer through the crop: crop offset plus the scaled position, giving an absolute position on the monitor as today. The host sees ordinary absolute input. Cursor position feedback is offset the same way.
- **The app moves or resizes on the PC.** The crop follows the next `window-list`. During a fast move the crop can lag the picture by up to one sampling interval.
- **The app is on another monitor than the stream**, for example while an elevation prompt is followed to its monitor or after the app was moved: the client shows the full desktop with a notice ("This app is on another display") and returns to the crop when the app is back on the streamed monitor.
- **The app spans two monitors.** The client crops the part on the streamed monitor.
- **The app is covered** by another window on the PC: the crop shows what covers it. Picking the app again brings it to the front.
- **The app closes.** The client returns to the full desktop and opens the switcher.
- **Follow active window** is paused while focus mode is on, because it would move the stream away from the app; leaving focus mode restores the user's setting.
- **UAC and lock screen.** The client shows the full secure desktop as today, then returns to the crop.
- **Leaving.** "Show full desktop" in the toolbar. It is also how the Start menu and taskbar are reached.

## Wire contract

Capability `windowList: 1` in `host-capabilities`. Clients hide focus mode when it is absent. Messages travel as `ctl` kinds on the existing control channel. The contract lives in a new shared fixture, `window-list-contract.json`, tested from Rust and JavaScript first; the Windows app and Android add consumer tests when they adopt it.

| Message | Direction | Carries |
| --- | --- | --- |
| `window-list` | Host to client | Every switchable window: `id`, `title`, `process`, `icon` (hash), `rect` in virtual-desktop pixels including owned popups, `monitor` selector, `state` (`foreground`, `normal`, `minimized`). Sent when focus mode is on and the list or a rectangle changes, at most four times a second. |
| `window-focus` | Client to host | Window `id`. Starts focus mode on the host side (list updates begin, follow pauses). |
| `window-focus-failed` | Host to client | Window `id` and `reason`: `gone`, `elevated` (needs the admin-input helper), `refused`. |
| `window-icon` | Client to host, then host to client | An icon hash; the answer carries a 32 px PNG, cached by the client for the session. |
| `focus-mode-off` | Client to host | Stops list updates and restores follow. |

Limits for the fixture: at most 64 windows per list, titles cut to 256 UTF-8 bytes, icons at most 8 KiB, the whole message well inside the relay's 512 KiB signaling limit (`SIGNAL_LIMIT` in `relay-rs/core/src/limits.rs`).

## Phases

| Phase | Ships | Checked by |
| --- | --- | --- |
| 1. Host | Window list, focus, icons, follow pause; the fixture | Unit tests for filtering, owned-popup union and message limits; fixture consumer test in Rust |
| 2. Browser | Switcher popover, crop, input mapping through the crop, notices and fallbacks | Fixture consumer test in JavaScript; a live run with a menu outside the window, a dialog, an app moved between monitors, a UAC prompt and the lock screen |
| 3. Windows app | The same in the Flutter session screen; the crop goes into the video processor's source rectangle | A live run as in phase 2 |
| 4. Android | The switcher as a sheet; crop and touch mapping | A device run |

## Unknowns

| Unknown | How to settle it | Phase |
| --- | --- | --- |
| Window rectangles in physical pixels | The main host process sets no per-monitor DPI awareness that this review found, so `GetWindowRect` may return scaled values. Check on a 150% display; use `DWMWA_EXTENDED_FRAME_BOUNDS` to leave out the invisible resize borders. | 1 |
| Finding owned popups | Menus belong to the app's thread rather than its owner chain. Collect visible top-level windows of the same thread and process that touch the app's rectangle; test with menus, context menus, combo boxes and an IME window. | 1 |
| Bringing a window to the foreground | Windows limits which process may take the foreground. Test from the host while it injects input, and through the admin-input helper for elevated apps. | 1 |
| Crop lag during a move | Measure with a fast drag; if visible, send rectangles more often while the app is moving. | 2 |

## Deferred: simultaneous monitors

Showing several host monitors at once, each in its own client window, is deferred. If it returns, try a **spanned desktop** first: the host composites all monitors into one frame on the existing stream, and each client window crops its monitor out of it, reusing the focus-mode crop. Frame size is the limit: NVENC caps H.264 at 4096 pixels wide, so two 1440p monitors need HEVC or downscaling, and browser decoder limits vary. None of this is verified.

The alternative, several streams per session, was sized on 2026-11-16 (desk review, nothing run on Windows). The findings, for whoever picks it up:

- **Host.** The session is one function, `run_serve` (about 5,900 lines). Its per-frame path is one block of about 2,500 lines sharing about 164 locals. Splitting it into a per-stream struct is a large refactor of the code that carries the first-frame and recovery invariants.
- **WSS.** ACK retirement treats any newer generation as retiring older frames (`host/src/link_capacity/ack_order.rs`), so ledgers must be per stream. The audio stall fence keys on the oldest un-ACKed video frame of any stream. A sent frame cannot be pre-empted, so a 200 KB background keyframe holds other streams about 270 ms at 6 Mbps.
- **WebRTC.** The host drops offers once ICE is connected (`should_forward_offer` in `host/src/host_session.rs`), so extra m-lines would have to be negotiated when the peer starts. Positions travel on the unordered `input-motion` channel with no retransmits, so they must carry their stream.
- **Capture.** One duplication per output per process: two streams cannot show the same monitor.
- **Encoders.** Secondary sources report 8 concurrent NVENC sessions on GeForce since January 2024 and 12 on recent drivers, per system and shared with other apps.
- **Windows app.** The frame target is one static that any window's session stop resets; the session has one front texture and one video rectangle; the app core starts once per process. Flutter 3.47.5 has no stable multi-view, so each window would need its own engine.
- **Browser.** A hidden main tab loses animation frames and gets throttled timers, which drive the pointer flush and the liveness ticks; any main-window `blur` releases input.

## Rejected: several apps at once

Two ways to show several apps at once were considered and rejected on 2026-11-21.

- **Per-window capture** (Windows.Graphics.Capture): a covered window cannot be clicked without raising it, menus and dialogs are separate windows and are missing, and every window needs its own encoder.
- **An app canvas**: a virtual monitor the apps are laid out on, which makes them all clickable at once. It needs a virtual display driver. The usable drivers either ask users to trust a third party's certificate machine-wide or lack runtime control. [SudoVDA](https://github.com/SudoMaker/SudoVDA), the best fit, is installed by [Apollo](https://github.com/ClassicOldSong/Apollo) by adding a self-signed code-signing certificate to the machine's Root and TrustedPublisher stores. [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) cannot add or remove one display and its reload crashes (issue #351). [LuminalVGD](https://github.com/NortheBridge/LuminalVGD) is Windows 11 only and alpha. Building and Microsoft-signing our own driver was judged not worth it.
