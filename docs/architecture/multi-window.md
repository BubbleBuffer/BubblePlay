# Multi-window streaming

Status: approved 2026-11-16 as a concept; nothing is built. Phase 0 spikes come first. Roadmap entry: [Multi-window streaming](../roadmap.md#multi-window-streaming).

Goal: a session carries several video streams instead of one. Two features build on that: one client window per host monitor, and an application mode that shows apps instead of the desktop, with a client-side app switcher. Related: [follow active window](follow-active-window.md), [monitor previews](monitor-previews.md), [session UI](session-ui.md), [file transfer](file-transfer.md).

Out of scope: independent seats (two cursors or two users on one PC), changing the host's display resolution, launching apps from the client, and macOS or Linux hosts. Drag and drop between client windows gets its own design later; this one keeps it possible.

## Current state

Every layer assumes one video per session. Follow active window, monitor previews and the edge-drop carry exist to work around that. Evidence is from docs, fixtures and a skim of the named sources on 2026-11-16; `host/src/main.rs`, which holds the session loop, was not traced.

| Area | Today | Where |
| --- | --- | --- |
| Session | One host, one viewer, one video | [README.md](README.md) |
| Capture | One DXGI duplication of the selected monitor; a switch happens in place with a new media generation | `host/src/capture.rs`, `host/src/host_capture.rs` |
| WSS media | 28-byte BPV4 header with no stream field; its reserved byte must be 0 | [wss-v4-contract.json](../../fixtures/wss-v4-contract.json) |
| WebRTC media | The viewer offers one video and one audio m-line; the host answers | `host/src/rtc.rs` |
| Relay | Checks media frames for size and direction only; header contents are the viewer's business | `relay-rs/core/src/protocol/mod.rs` (`validate_media_binary`) |
| Pointer input | Absolute u16 position relative to the streamed monitor, mapped to the virtual desktop on the host | `host/src/input.rs` |
| File drop | Binds session, media generation, viewport and drop position | [file-transfer.md](file-transfer.md) |
| Windows app video | The Rust core decodes into a shared D3D11 texture; one Flutter view per window shows it; input comes from that window's messages | `app/windows/runner/stream_plugin.h`, `app/windows/runner/flutter_window.cpp` |

## Decisions (2026-11-16)

| Decision | Choice |
| --- | --- |
| WSS transport for extra streams | Multiplex on the existing media socket. Older relays keep working. The host orders frames by priority. |
| Stream count | Target three (for example two monitors and the app canvas), lowered if the encoder spike shows the target GPUs cannot run that many sessions. |
| Controls | Every client window has its own toolbar: one component, three variants (desktop, monitor, app). In the Windows app that is one Flutter engine per window. |
| Frame of an app window | Keep the local title bar; the app's own title bar shows inside the picture. |
| Application mode | The app canvas is the goal. The single-app crop ships first and stays as the fallback; the canvas is skipped if the driver spike fails. Per-window capture is not used. |
| Android | App switcher and single-app view only; no extra windows. |

## Streams

A stream is what a session's video half is today, made countable. Stream 0 is the existing video and behaves exactly as now, so an old client or host sees no change.

- **A stream owns** an ID (one byte), a source (a physical monitor or the app canvas), its own generation and sequence space, its own encoder, and its own first-frame handshake.
- **Per session, as today:** control messages, the input channel, audio, clipboard, the file socket, cursor shape and the session epoch.

**WSS media.** The header's reserved byte carries the stream ID once both sides advertise `multiStream`. Stream 0 packets stay byte-identical to BPV4. `ack` and `keyframe` messages gain an optional `stream` field; absent means 0. The relay needs no change. One socket gives one send queue and one congestion window, so the host decides which frame goes next:

1. PC sound, which shares the media socket on WSS.
2. The stream under the pointer.
3. The other streams. The host never starts a background frame while a pointer-stream frame waits, and staggers keyframes across streams.

A lost TCP segment stalls every video stream on the socket. Input, control and files are unaffected: they have their own sockets. Separate media sockets per stream remain the fallback if the phase 0 measurement shows stalls.

**WebRTC media.** The viewer offers one receive-only video m-line per stream on the existing peer; the host reports which m-line carries which stream. Opening or closing a stream renegotiates in place, without a new signaling epoch.

**Pointer input.** Packets keep their format. A new reliable packet, `pointer-target`, names the stream the pointer is on; the client sends it when the pointer enters a different client window. Later absolute positions map through that stream's source rectangle. It is legal while a button is held, and held input carries across it as across a monitor switch today. Keys and relative motion are unchanged; the keyboard goes to host focus.

**Bitrate and encoders.** The link-capacity estimate is split across streams, the largest share to the stream under the pointer and a floor for the rest. The host advertises `maxStreams` and refuses a stream it cannot encode, with a reason the client shows; it never degrades stream 0 to make room.

**Lifecycle.** The client asks to open a stream for a source. The host answers with the stream ID, source and size, or refuses. The stream runs the existing first-frame handshake, and input opens for it once its first frame is admitted. Either side can close a stream; closing stream 0 ends the session.

## Multi-monitor windows

Each host monitor can open as its own stream in its own client window: one duplication and one encoder per output.

- **Opening.** The Monitors popover gains "Open in new window" per monitor and "Use all my screens".
- **Placement.** The client remembers per PC which host monitor went to which local screen. With more host monitors than local screens, the user picks.
- **Pointer.** Entering another client window sends `pointer-target`, then positions; the host cursor jumps to that monitor.
- **Closing.** Closing a monitor window closes its stream. Closing the main window ends the session.
- **Follow active window** moves stream 0 only and skips monitors that have their own window. With every monitor open it is off.
- **Monitor previews** are sent only for monitors without a live stream.
- **UAC and lock screen** show on stream 0 as today. Other windows keep their last frame under the secure-desktop badge.

| Client | Extra windows | Limits |
| --- | --- | --- |
| Browser | The main tab owns the session, sockets and decoders. Each extra window is a same-origin popup, opened from a click, that the main tab draws into. The Window Management API places popups fullscreen on chosen screens after a permission prompt. | Keyboard lock and pointer lock are per document, so Game mode runs in one window at a time. Popups need a click each unless the permission is granted. |
| Windows app | Each extra window is another Flutter window in the same process with its own engine, showing the session screen for one stream. The runner repeats its pair of Flutter view controller and stream plugin bound to the window handle. The Rust core stays the single owner of the session. | A second engine next to the Rust bridge is untested. The frame-ready callback is one global today and must become per window. |
| Android | Not offered. | One stream. |

## Application mode

Application mode shows apps instead of the desktop. It is a view onto one real desktop, not a second seat: the host has one cursor and one focus. A click in a client window moves the real cursor onto that app and Windows makes it the foreground window; the keyboard follows the app clicked last. Several apps can be clickable at once only if they do not overlap on the host, which is what the app canvas is for.

| Approach | Good | Bad |
| --- | --- | --- |
| Crop a physical monitor's stream to the window | No new capture path and no driver. Menus, tooltips and IME popups are in the frame. | Correct only while the window is not covered, so one app at a time. |
| App canvas: a virtual monitor with the apps side by side, cropped per app | Nothing overlaps, so every app is visible and clickable. The real cursor works as normal. Existing capture path, one encoder. Popups are in the frame. | Needs a virtual display driver installed with admin rights. The apps leave the physical screens while in use. |
| Windows.Graphics.Capture per window (not used) | No driver. Shows a covered window. | A covered window cannot be clicked without raising it. Menus and dialogs are separate windows and are missing. One encoder per window. Nothing while minimized. |

### Single-app view (first)

No new capture and no new encoder. The host keeps streaming the monitor and sends each window's rectangle, tagged with the capture timestamp; the client crops the decoded frame. The host's quality map can favour the focused rectangle. The rest of the desktop is still sent, which costs bandwidth when something else animates.

- **Focus.** Picking an app sends `window-focus`; the host restores it if minimized and brings it to the foreground. Elevated windows go through the admin-input helper.
- **Resize.** Resizing the client window sends a debounced `window-resize`; the host resizes the app window to match, one host pixel per client device pixel where possible. If the app refuses the size, the client letterboxes.
- **Popups outside the window.** The crop grows to the union of the app's main window and the visible windows it owns.
- **Dialogs and extra windows.** Owned windows join their owner's view. Unowned windows of the same app are separate switcher entries.
- **Closed window.** Its client window closes; after the last one the client returns to the switcher.
- **UAC and lock screen.** The client drops to the full-desktop view of the secure desktop, then returns.
- **Escape hatch.** "Show full desktop" is always one click away; it is also how the Start menu and taskbar are reached.
- **Protected content** shows black, and the client says why.

### App canvas (goal)

A virtual monitor that exists only while a session uses it. It reuses the window list, crop, focus and resize from the single-app view and adds a layout manager on the host.

- **Pointer.** The canvas is a real monitor with the real cursor. A click in any client window lands on that app's tile; hover and the wheel work on background apps too.
- **Layout.** Each app gets a tile the size of its client window, with a gutter for menus and tooltips. Resizing a client window resizes its tile and re-flows the others.
- **One stream.** The canvas is captured and encoded once; client windows crop it, so application mode uses one encoder however many apps are open.
- **Canvas size** grows with the tiles, up to the largest picture the encoder and the client's decoder accept. Past that the host refuses another app.
- **Maximize.** The host catches it, restores the tile, and asks the client to maximize that client window instead.
- **Getting the apps back.** Windows moves windows off a monitor that disappears. The canvas is removed when the session ends, and by the tray if the host dies.
- **Someone at the PC.** Apps on the canvas are not on the physical screens. "Show full desktop" brings them back.
- **Without the driver** the host does not advertise `appCanvas`, and the client offers the single-app view only.

Driver candidate: [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver), MIT-licensed, built on Microsoft's indirect display framework, with releases signed through SignPath for Windows 10 and 11. Its documentation describes control through a settings file and a companion app; runtime create, resize and remove from the host is unverified. Our own driver would need Microsoft driver signing.

## App switcher

The client draws its own switcher from a window list the host sends; the host's Alt+Tab screen would be outside a cropped picture.

- **`window-list`**: one entry per switchable window with ID, title, process name, icon hash, rectangle in virtual-desktop coordinates, monitor and state. Sent on change. Filtering follows the follow-active-window rules: no invisible, cloaked, shell or ghost windows. Icons are small PNGs fetched once per hash; thumbnails reuse the monitor-preview mechanism and its byte budget.
- **App strip** in the toolbar: click to focus; a secondary action pops the app out into its own window.
- **Switcher overlay**: thumbnails with titles; arrows and Enter pick, Esc closes.
- **Shortcuts.** With keyboard lock on, the client intercepts Alt+Tab in application mode and opens its overlay; in full-desktop mode Alt+Tab goes to the host. With keyboard lock off, the overlay opens with a client chord in the Ctrl+Alt+Shift family the Windows app reserves.
- **Android** shows the switcher as a sheet from the session bar; a pick shows that app as a single stream.

## Controls in each window

Session controls appear in every window and show the same state everywhere.

| Control | Acts on | Desktop window | Monitor window | App window |
| --- | --- | --- | --- | --- |
| Display or app picker | This window | Monitors popover, plus "Open in new window" | Which monitor this window shows | App name and icon; opens the switcher |
| Fullscreen, zoom, stats | This window | Yes | Yes | Yes |
| Resizing the window | This window | Scales the picture | Scales the picture | Resizes the app on the PC |
| Close button | This window | Ends the session | Closes this window and its stream | Closes this view; the app keeps running |
| Work/Game, sound, quality, keyboard lock | Session | Yes | Yes | Yes |
| Files, shortcuts, settings | Session | Yes | Yes | Yes |
| Show full desktop / Show apps | Session | Yes | No | Yes |
| Disconnect | Session | Yes | Yes | Yes |

Only the main window's close button ends the session. Stats are per stream, with link totals in every window. Game mode captures one window at a time; switching it on in another window moves it there.

## Drag and drop

| Case | Today or later | What this design keeps possible |
| --- | --- | --- |
| Local file onto a remote app | Exists as a native drop | The `drop` binding gains `stream`; with a client-side crop the client adds the crop offset. |
| Remote app to remote app across two client windows | Later | `pointer-target` with a button held. On the app canvas both apps sit on one monitor without overlap, so the host sees an ordinary drag. Across physical monitors, raising the target window mid-drag is unknown. |
| Remote app to the local machine | Downloads are offers | Nothing; out of scope. |

## Wire contract sketch

Everything new is capability-gated; stream 0 without the capability is today's protocol. Names are proposals until two new shared fixtures (`multi-stream-contract.json`, `window-list-contract.json`) exist with consumer tests in Rust, JavaScript and Kotlin.

| Capability | Advertised by | Meaning |
| --- | --- | --- |
| `multiStream: 1`, `maxStreams: n` | Host and client | More than one video stream per session |
| `windowList: 1` | Host | Window list, focus and resize |
| `appCanvas: 1` | Host | A virtual display driver is installed and the canvas can be created |

| Message | Direction | Carries |
| --- | --- | --- |
| `media-control` `stream-open` | Client to host | Source: a monitor selector, or the app canvas |
| `stream-opened` / `stream-refused` | Host to client | Stream ID, source, size and WebRTC m-line; or a reason such as `encoder-budget` |
| `media-control` `stream-close` | Either | Stream ID |
| `window-list` | Host to client | Entries as above, with a capture timestamp for the rectangles |
| `window-focus` | Client to host | Window ID |
| `window-resize` | Client to host | Window ID, width and height in host pixels |
| `canvas-apps` | Client to host | Window IDs to place on the canvas with their client window sizes; the host answers with each tile's rectangle |
| `window-icon` | Host to client | Icon hash and PNG, on request |

| Channel | Change |
| --- | --- |
| WSS frame header | Reserved byte = stream ID; 0 is byte-identical to BPV4 |
| WSS `ack`, `keyframe` | Optional `stream`; absent means 0 |
| WebRTC | One receive-only video m-line per stream on the existing peer |
| Input | New reliable packet type 14, `pointer-target`, with the stream ID; types 0 to 13 unchanged |
| File drop | `drop` gains `stream` |

## Phases

| Phase | Ships | Checked by |
| --- | --- | --- |
| 0. Spikes | Answers to the unknowns below; no product change | A short written result per spike |
| 1. Streams and multi-monitor | Stream IDs in control, media and input. One duplication and encoder per monitor. Browser popups and placement. Extra Flutter windows in the Windows app. | New fixture with consumer tests; the fake host serving two streams; a live two-monitor run on WSS and WebRTC |
| 2. Window list, switcher, single-app view | Window list and icons; focus and resize; client-side crop; app strip and overlay in the browser, Windows app and Android | Window-list fixture; unit tests for filtering and crop mapping; a live run with a resized app, a menu outside the window and a UAC prompt |
| 3. App canvas | The driver in the host installer; the canvas, its layout manager and a crop per client window; pop-out from the app strip | Live runs with three apps on the canvas, a maximize, an elevated app, and a killed host whose apps must return to the physical screens |

Phase 3 is skipped if the driver spike fails.

## Unknowns

| Unknown | How to settle it | Phase |
| --- | --- | --- |
| Concurrent NVENC sessions on the target GPUs, which sets `maxStreams` | Open sessions until refused; check the software encoder for a low-rate background stream | 0 |
| Cost of two or three duplications and encodes | Run `host/scripts/probe-host-performance.mjs` with two streams | 0 |
| Head-of-line delay on one WSS socket | Measure stream 0 frame delay while stream 1 sends a keyframe, on a slow link | 0 |
| Size of the session-loop change in `host/src/main.rs` | Read it and size the change before committing to phase 1 | 0 |
| Browser main tab hidden behind popups (throttled timers, no animation frames) | Prototype: decode in the worker, present on the popup's own frame callback | 0 |
| Two Flutter engines sharing the Rust bridge | Open a second window from the runner showing the session screen; measure memory and time to open | 0 |
| Virtual display driver | Add, resize and remove a monitor from the host at runtime; install without test mode; capture it with the existing path. Then choose a driver or skip the canvas | 0 |
| Window placement permission in the browser | Try the prompt; define the fallback of plain popups the user moves | 1 |
| Bringing a window to the foreground | Test from the host while it injects input, and through the admin helper | 2 |
| Crop and pixels out of step during a window move | Tag rectangles with the capture timestamp; test a fast drag | 2 |
