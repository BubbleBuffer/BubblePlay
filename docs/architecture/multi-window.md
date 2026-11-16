# Multi-window streaming

Status: approved 2026-11-16 as a concept; nothing is built. Phase 0 desk results (2026-11-16) are [below](#phase-0-results); the live spikes on Windows hardware are still open. Roadmap entry: [Multi-window streaming](../roadmap.md#multi-window-streaming).

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
| Application mode | The app canvas is the goal. The single-app crop ships first and stays as the fallback; the canvas is skipped if the driver spike fails. Per-window capture is not used. Which driver, and how users come to trust it, is [open](#open-decision-driver-trust). |
| Android | App switcher and single-app view only; no extra windows. |

## Streams

A stream is what a session's video half is today, made countable. Stream 0 is the existing video and behaves exactly as now, so an old client or host sees no change.

- **A stream owns** an ID (one byte), a source (a physical monitor or the app canvas), its own generation and sequence space, its own encoder, and its own first-frame handshake.
- **Per session, as today:** control messages, the input channel, audio, clipboard, the file socket, cursor shape and the session epoch.

**WSS media.** The header's reserved byte carries the stream ID once both sides advertise `multiStream`. Stream 0 packets stay byte-identical to BPV4. `ack` and `keyframe` messages gain an optional `stream` field; absent means 0. The in-flight ledger, ACK retirement and capacity samples are kept per stream: today a newer generation retires every older frame in flight (`host/src/link_capacity/ack_order.rs`), which would let one stream's ACK retire another's frames. The link-capacity estimate stays per socket. The relay needs no change. One socket gives one send queue and one congestion window, so the host decides which frame goes next:

1. PC sound, which shares the media socket on WSS.
2. The stream under the pointer.
3. The other streams. The host never starts a background frame while a pointer-stream frame waits, and staggers keyframes across streams.

A frame already handed to the socket cannot be pre-empted: each access unit is one WebSocket message, and messages do not interleave. A background frame therefore delays the pointer stream by its size over the link rate, about 270 ms for a 200 KB keyframe at 6 Mbps and over 2 s at 700 kbps. So the pointer stream has first claim on the in-flight window, background streams use only what is left, and on a constrained link they freeze on their last frame. The audio stall fence keys on the oldest un-ACKed frame of stream 0, not of any stream, so a stalled background stream cannot mute PC sound. A lost TCP segment stalls every video stream on the socket. Input, control and files are unaffected: they have their own sockets. Separate media sockets per stream remain the fallback if the phase 0 measurement shows stalls.

**WebRTC media.** The viewer offers `maxStreams` receive-only video m-lines when the peer starts; unused ones carry nothing until the host assigns a stream to them and reports which m-line carries it. Opening a stream therefore needs no renegotiation, which the host does not support: it drops offers once ICE is connected (`should_forward_offer` in `host/src/host_session.rs`). A failed first-frame handshake closes that stream only; the existing peer restart applies to stream 0.

**Pointer input.** Positions say which stream they belong to. Two new packet types repeat absolute-v2 and buttons-at with a stream byte; types 0 to 13 stay and mean stream 0. A separate "pointer is now on stream N" message would not work: on WebRTC positions travel on the unordered `input-motion` channel with no retransmits (`worker/public/peer.js`), so they can overtake a reliable switch message. The host keeps one absolute region per stream and selects it per packet, without the resets `set_absolute_region` does today. A press on one stream and its release on another is legal, and held input carries across as across a monitor switch today. Keys and relative motion are unchanged; the keyboard goes to host focus. Cursor-state feedback (position and visibility) is reported per stream, because it is relative to the stream's source.

**Bitrate and encoders.** The link-capacity estimate is split across streams, the largest share to the stream under the pointer and a floor for the rest. The host advertises `maxStreams` and refuses a stream it cannot encode, with a reason the client shows; it never degrades stream 0 to make room.

**Lifecycle.** The client asks to open a stream for a source. The host answers with the stream ID, source and size, or refuses. The stream runs the existing first-frame handshake, and input opens for it once its first frame is admitted. Either side can close a stream; closing stream 0 ends the session.

## Multi-monitor windows

Each host monitor can open as its own stream in its own client window: one duplication and one encoder per output.

- **Opening.** The Monitors popover gains "Open in new window" per monitor and "Use all my screens".
- **Placement.** The client remembers per PC which host monitor went to which local screen. With more host monitors than local screens, the user picks.
- **Pointer.** Positions in another client window carry that window's stream; the host cursor jumps to that monitor.
- **One stream per monitor.** A process can hold one duplication per output. A request that would put stream 0 on a monitor that already has its own stream (a pick, follow active window, the elevation-prompt follow) brings that monitor's window to the front instead.
- **Closing.** Closing a monitor window closes its stream. Closing the main window ends the session.
- **Follow active window** moves stream 0 only and skips monitors that have their own window. With every monitor open it is off.
- **Monitor previews** are sent only for monitors without a live stream.
- **UAC and lock screen** show on stream 0 as today. Other windows keep their last frame under the secure-desktop badge.

| Client | Extra windows | Limits |
| --- | --- | --- |
| Browser | The main tab owns the session and sockets. Each extra window is a same-origin popup, opened from a click. On WSS the main tab hands a canvas from the popup to that stream's decode worker as an OffscreenCanvas and forwards the stream's frames; the worker decodes and draws, as the WSS path already does. On WebRTC the popup gets a `<video>` with a MediaStream of that stream's track. The Window Management API places popups fullscreen on chosen screens after a permission prompt. | A hidden main tab gets no animation frames and throttled timers, so the pointer flush, viewport sync and the liveness, recovery and heartbeat ticks move off the main tab's `requestAnimationFrame` and timers (into a worker clock, or the rAF of the window that took the event). Input is released only when no session window has focus; today any main-window `blur` releases it, and clicking a popup blurs the main window. Keyboard lock and pointer lock are per document, so Game mode runs in one window at a time. |
| Windows app | Each extra window is another `FlutterViewController`, with its own engine, made by the runner with a role (main or stream N) and its own `StreamPlugin`. The second engine starts a separate Dart entry point and calls `RustLib.init` itself; the Rust core stays the single owner of the session. Flutter 3.47.5 has no stable multi-view support, and `desktop_multi_window` moves the UI to a separate thread, which the core's synchronous calls do not allow. | Today the frame target is one static that any window's session stop resets, the session has one front texture and one video rectangle, and the app core starts once per process with one change subscriber. These become per stream and fan out to several windows. One window owns input capture; the others only present, so their ticks cannot release the capture on a focus check. |
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

The driver is chosen in phase 0; see [Phase 0 results](#phase-0-results).

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
| Remote app to remote app across two client windows | Later | Stream-tagged positions with a button held. On the app canvas both apps sit on one monitor without overlap, so the host sees an ordinary drag. Across physical monitors, raising the target window mid-drag is unknown. |
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
| `stream-opened` / `stream-refused` | Host to client | Stream ID, source, size and the pre-negotiated WebRTC m-line it uses; or a reason such as `encoder-budget` or `monitor-has-stream` |
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
| WebRTC | `maxStreams` receive-only video m-lines offered when the peer starts; no renegotiation |
| Input | Two new packet types: absolute-v2 and buttons-at with a stream byte; types 0 to 13 unchanged and mean stream 0 |
| File drop | `drop` gains `stream` |

## Phases

| Phase | Ships | Checked by |
| --- | --- | --- |
| 0. Spikes | Answers to the unknowns below; no product change | A short written result per spike |
| 1a. Session-loop refactor | The per-frame path of `run_serve` moved into a per-stream struct and function, with stream 0 only and no behavior change | The existing host tests and the WSS and RTC qualification runs, unchanged |
| 1. Streams and multi-monitor | Stream IDs in control, media and input. One duplication and encoder per monitor. Browser popups and placement. Extra Flutter windows in the Windows app. | New fixture with consumer tests; the fake host serving two streams; a live two-monitor run on WSS and WebRTC |
| 2. Window list, switcher, single-app view | Window list and icons; focus and resize; client-side crop; app strip and overlay in the browser, Windows app and Android | Window-list fixture; unit tests for filtering and crop mapping; a live run with a resized app, a menu outside the window and a UAC prompt |
| 3. App canvas | The driver in the host installer; the canvas, its layout manager and a crop per client window; pop-out from the app strip | Live runs with three apps on the canvas, a maximize, an elevated app, and a killed host whose apps must return to the physical screens |

Phase 3 is skipped if the driver spike fails.

## Phase 0 results

Desk results from 2026-11-16: code reading in this repository and the primary sources linked below. Nothing here ran on Windows. learn.microsoft.com and NVIDIA's documentation pages were not reachable from the research environment; Microsoft references were read from their GitHub mirrors (`MicrosoftDocs/win32`, `MicrosoftDocs/sdk-api`, `MicrosoftDocs/windows-driver-docs`).

### Session loop

The session is one function, `run_serve` in `host/src/main.rs` (about 5,900 lines), on one thread with one `'session` loop. Its per-frame path is a single block of about 2,500 lines with 20 `continue 'session` exits, sharing about 164 function-level locals. Capture, the encoder, generation and sequence, IDR and first-frame state, the pacer, tile and diff analyzers are all single locals; three macros (`switch_capture_monitor!`, `park_viewer!`, `begin_viewer_input!`) mix per-stream and per-session steps.

- **Size: large.** About 5,000 to 6,000 lines are touched, and the block carries the first-frame and recovery invariants. Hence phase 1a: move the per-frame path into a per-stream struct with stream 0 only, prove no behavior change, then add streams.
- **Per stream:** the duplication and its recovery; encoder, scaler, profile transition and IDR replay; generation and sequence; first-frame, startup, transition and viewport state; pacer, tile quality and temporal diff; stats; on WSS a send ledger, on WebRTC a Mid, payload type and access-unit slot.
- **Per session:** signaling, input sink and coalescer, carried input, audio, files, previews, follow and elevation-prompt state, the secure helper and desktop watcher, link capacity and the rate controller (with a share allocator), and the RTC peer.
- **Capture** objects are instance-scoped and a monitor switch already holds two duplications briefly, so duplications of different outputs look feasible. `LocalCaptureSource` bundles the desktop watcher with the one duplication and must be split. The NVENC native-call observer is one per process and thread.
- **Design changes made from this:** per-stream ACK ledgers, the audio stall fence on stream 0, m-lines negotiated up front, stream-tagged position packets, one stream per monitor, and per-stream cursor feedback (all above).

### WSS head-of-line delay

Answered by arithmetic and the existing admission code (`host/src/wss_v4.rs`): sending is admission-controlled before encode against a window of 150 to 400 ms of airtime or 4 to 12 frames, scaled by RTT. A sent frame cannot be pre-empted. The resulting priority rule is above under Streams. Still to measure live: the real keyframe sizes of a background stream, and whether the frozen-background rule triggers too often on the relay path.

### Windows app and browser windows

The changes are recorded in the clients table above. Still needs a live test on Windows: two engines with FRB init and the change stream; memory and time to open a window; one shared texture opened by several engines; capture moving between windows; closing a secondary window while frames arrive; exit order. In Chromium, with the main tab minimised, covered, and hidden for more than five minutes, sound on and off: popup canvases keep drawing, worker timers keep their rate, `requestVideoFrameCallback` runs in the popup, WebSocket delivery keeps up, tab freezing and Memory Saver leave the main tab alone, pointer and keyboard lock work in a popup, and window placement works.

### NVENC and the software encoder

- **NVENC sessions.** NVIDIA's support matrix could not be opened. Secondary sources report 8 concurrent sessions on GeForce since January 2024, and 12 on recent drivers (591.44 and later, December 2025). The limit is per system and enforced by the driver, so OBS, Discord, browsers and Game Bar count against it. Three streams fit either way; the host must refuse a stream when an encoder will not open, which the design already does.
- **Microsoft software H.264** is usable for a low-rate background stream: NV12 input (a BGRA to NV12 conversion), low-latency mode by default, no documented size limit. It is not supported on Windows Server.
- **Desktop Duplication** allows one duplication per output per process and at most four duplicating processes per session.

Still to measure: three encodes at once next to OBS and Discord, recording the driver version; the software encoder's CPU at canvas size and 1 to 5 fps.

### Virtual display driver

A monitor created by an indirect display driver appears to DXGI as an output of the driver's render adapter, and Desktop Duplication captures it: [Apollo](https://github.com/ClassicOldSong/Apollo) (a Sunshine fork) does exactly this, setting the driver's render GPU to the encoding GPU. Hybrid laptops are untested.

| Driver | Runtime control | Trust and license | Fit |
| --- | --- | --- | --- |
| [SudoVDA](https://github.com/SudoMaker/SudoVDA) | Device I/O: add a display of any size and refresh rate by an ID, remove it, set the render GPU. A non-admin process can open the device. A watchdog removes every virtual display when not pinged for 3 s. Up to 10 displays. | Self-signed (`CN=sudovda@su.mk`); Apollo's `install.bat` adds that certificate to the machine's Root and TrustedPublisher stores. It is a code-signing certificate, so its key holder can sign code that PC then trusts. No LICENSE file; the README says "MIT and CC0 or Public Domain". The public source (April 2025) is older than the binary Apollo ships (1.10.9.289, July 2025). A Windows 11 reliability problem (WUDFHost hangs) is reported by another driver's README. | Best API fit |
| [Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) | Named pipe open to everyone (`SETDISPLAYCOUNT`, `RELOAD_DRIVER` and others). Modes and count come from `C:\VirtualDisplayDriver\vdd_settings.xml`, which needs admin to write. No add or remove of one display, no arbitrary size; every reload passes the pipe handle where the device is expected (open issue #351, reload crashes). | MIT. Signed through SignPath; the companion installer adds the signer certificate to TrustedPublisher and Root. | Poor: every resize is a driver reload |
| [LuminalVGD](https://github.com/NortheBridge/LuminalVGD) | Control from SYSTEM or elevated admins only, so through the elevated helper. Delivers frames mainly through its own shared memory; Desktop Duplication is a fallback. | AGPL-3.0 (with a commercial option). Commercially signed; certificate added to TrustedPublisher only. Windows 11 only, alpha. | Possible later |
| [parsec-vdd](https://github.com/nomi-san/parsec-vdd) | Add, remove and update displays; a preset list plus up to five custom modes. | Proprietary Parsec driver binary. | Poor: fixed sizes |

None of these can be shipped as they are without asking users to trust a third party's certificate machine-wide. That conflicts with the project's trust model, so the driver choice is an [open decision](#open-decision-driver-trust). Microsoft's rules for loading a user-mode driver signed outside Microsoft's programs were not checked against Microsoft's own pages.

Live checks for whichever driver is chosen: install on Windows 10 22H2 and Windows 11 24H2 or later with Secure Boot, Memory Integrity and Smart App Control on and test signing off; from the non-elevated host, add displays at odd sizes (2400x1350, 3000x1000) and time their arrival; check the DXGI output's adapter is the NVIDIA adapter after setting the render GPU, and that duplication delivers frames (also on a hybrid laptop); three NVENC encodes on that device; resize by remove and re-add and by a mode change, with duplication recovering; move normal, maximized and minimized windows onto the canvas, and confirm the non-elevated host cannot move an elevated app's window; remove the display and check every window returns; kill the host and check the watchdog removes the canvas; coexistence with an installed Apollo, which uses the same hardware ID.

### Open decision: driver trust

| Option | Cost |
| --- | --- |
| Build a SudoVDA-derived driver from source (Microsoft's MIT-licensed sample plus SudoVDA) and have it attestation-signed by Microsoft | No certificate added to users' trust stores. Needs an EV code-signing certificate and a Partner Center account; we maintain a driver. Whether attestation signing suffices for this driver class on Windows 10 and 11 is unverified. |
| Ship SudoVDA as Apollo does | Fastest; adds a third party's code-signing certificate to every user's Root store. |
| LuminalVGD | License-compatible (AGPL), but Windows 11 only, alpha, and frames mainly through its own channel. |
| Skip the app canvas | Application mode stays the single-app view. |

Recommendation: the first option if an EV certificate is acceptable, otherwise skip the canvas until one is. Not the second.

### Still open from the original list

| Unknown | How to settle it | Phase |
| --- | --- | --- |
| Window placement permission in the browser | Try the prompt; define the fallback of plain popups the user moves | 1 |
| Bringing a window to the foreground | Test from the host while it injects input, and through the admin helper | 2 |
| Crop and pixels out of step during a window move | Tag rectangles with the capture timestamp; test a fast drag | 2 |
