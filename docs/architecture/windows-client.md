# Native Windows client

Status: proposed 2026-10-13 (phase 0, stack choice waiting for approval). Nothing is built yet; `client-win/` holds only the build-output junction.

## Goal

A native Windows viewer that feels clearly better than the browser receiver: low latency and smooth frame pacing, hardware decode, full keyboard capture (Win, Alt+Tab, Ctrl+Esc go to the remote PC while focused, with an obvious escape chord), proper pointer capture, a fast polished UI (connect screen, saved devices, in-session chrome) and audio.

v1 scope: today's login and approval flows, video, mouse/keyboard/scroll input, audio, monitor selection, and session chrome matching [session UI](session-ui.md). File transfer, clipboard and the rest come after v1.

Constraint: no host, relay or protocol changes. The client speaks exactly what the browser receiver speaks.

## What the client must speak

Learned from `worker/public/`, `relay/`, `worker/src/` and `host/src/` (research notes 2026-10-13; line references are in those files, not repeated here).

- **Login.** `POST /__auth/login` with JSON `{password, totp?, remember}` and `Origin` equal to the relay's public URL. The relay sets two cookies, `__Host-gate` and `__Host-bp-session`, which go on every socket upgrade. The Go relay accepts password plus TOTP; the Cloudflare relay needs an approved site-gate cookie first (ntfy or [host approval](browser-approval.md)), so the client needs a "waiting for approval" screen. There is no pairing today; [device keys](device-keys.md) is designed but not built.
- **Signaling.** `wss://<relay>/ws`, first message `{"type":"hello","role":"client"}`. The reply `hello-ok` carries `iceServers` and a fresh `mediaEpoch`; every media socket and `media-control` message must carry that epoch. Reconnect is a new hello and a new epoch.
- **Two media transports, both already served by the host:**
  - **WebRTC** (browser default in auto mode): the client offers recvonly HEVC + H.264 video and Opus audio, and opens two data channels, `input-control` (ordered, reliable) and `input-motion` (unordered, no retransmits). The host is str0m 0.21, prefers HEVC, gathers host and server-reflexive candidates only (no TURN client on the host side).
  - **WSS v4**: `/media-ws?epoch=E`. One Annex-B access unit per binary message behind a 28-byte `BPV4` header; the client ACKs every frame and asks for keyframes on gaps. Audio rides the same socket as BPA1 packets (one 10 ms Opus frame each) when `start` carries `audio:1`. Input goes as binary on `/ws`. Traffic passes through the relay.
- **Input.** Little-endian binary packets from `fixtures/input-contract.json`: set-1 scan codes with an E0 flag (type 13), relative motion with wheel (7), absolute position (8), buttons-at (9), release-all (6), with one wrapping u16 sequence per epoch. Cursor state and cursor kind come back as packets 128 and 129.
- **Monitor switch.** `media-control` `monitor`. The browser's in-place switch needs `requestVideoFrameCallback` RTP timestamps; a native client can match the admitted `rtpTimestamp` itself on WebRTC, or restart in a new epoch as the browser's fallback does.

Nothing in the protocol depends on a browser. Cookies, the Origin header and the ntfy wait are plain HTTP.

## Stacks compared

### A. All-Rust: str0m + WSS v4, D3D11 decode, D3D11 flip-model present, egui on D3D11

- **Transport:** WSS v4 with `tokio-tungstenite`, and WebRTC with str0m, the same crate the host uses. str0m is sans-IO and does no decoding; it depacketizes H.265 (RFC 7798) and H.264 into NAL units, and the client groups them into access units.
- **Decode:** hardware decode straight into a D3D11 NV12 texture. Two ways, to be settled by the spike below:
  - Media Foundation decoder MFT with a D3D11 device manager. H.264 is in-box. HEVC needs the Store "HEVC Video Extensions" package (secondary sources; not confirmed on Microsoft Learn), so on PCs without it the client offers H.264 only. `CODECAPI_AVLowLatencyMode` must be set (as VT_UI4 on the H.264 MFT).
  - The D3D11 video decoding API (`ID3D11VideoDecoder`, DXVA), which uses the GPU driver and needs no Store package, as FFmpeg's d3d11va does. Doing this by hand means parsing slice headers and filling DXVA picture parameters for HEVC and H.264; through FFmpeg it means an LGPL dynamic link.
- **Latency path:** socket or UDP, depacketize, submit the access unit, decoder output texture, one shader pass (NV12 to RGB, scale) into the back buffer, Present. No jitter buffer: frames are shown as soon as they decode, which is the main latency win over the browser's `<video>` path.
- **Frame pacing:** flip-model HWND swap chain with `ALLOW_TEARING` and a frame-latency waitable object at latency 1. Two present modes: "lowest latency" presents on arrival with tearing allowed; "smooth" presents at the next vblank and drops all but the newest decoded frame. Windowed tearing is supported for flip-model HWND swap chains; borderless full screen gets independent flip.
- **Keyboard capture:** a `WH_KEYBOARD_LL` hook installed only while the window is focused and the session is active. It sees Win, Alt+Tab, Ctrl+Esc, Alt+F4 and Ctrl+Shift+Esc and swallows them after forwarding the scan code. Ctrl+Alt+Del and Win+L cannot be intercepted by any app. The hook callback only queues and returns, because Windows silently removes a hook that exceeds `LowLevelHooksTimeout`. SDL's keyboard grab works the same way. Escape chord: Ctrl+Alt+Shift+Q to leave the session (as Moonlight does) and Ctrl+Alt+Shift+Z to release or recapture mouse and keyboard; the chord is shown in the chrome.
- **Pointer capture:** Raw Input for relative motion (motion-v2), `ClipCursor` plus a hidden local cursor in captured mode, and absolute mode (absolute-v2) with the remote cursor kind mapped to a local cursor.
- **Audio:** `opus` crate (libopus, BSD-3) and WASAPI shared mode with a small buffer through `cpal` or the `windows` crate.
- **UI:** egui rendered with `egui-directx11` (0.13, Sept 2026) on the same device and swap chain as the video, so the chrome is composited without a second window. egui's default look is a developer tool. Making it polished means custom styling from the BubblePlay tokens, the product fonts, Lucide icons as SVG and hand-tuned animation. A good result is achievable, but the polish ceiling is lower than a native Fluent UI's.
- **Binary size:** one exe, roughly 6-12 MB (estimate, unmeasured), plus FFmpeg DLLs (~20 MB) only if that decode route is chosen.
- **Licensing:** all MIT/Apache/BSD; LGPL only with FFmpeg.
- **Effort:** largest plumbing share of the three, but each piece is small and testable: WSS v4 first (no ICE), then WebRTC.

### B. Rust core with a WinUI 3 shell (C#), video in a SwapChainPanel

- **Transport, decode, input, audio:** as in A, in a Rust DLL behind a C ABI.
- **Latency path:** SwapChainPanel takes a composition swap chain, so every frame goes through DWM composition. Tearing for composition swap chains is not documented as supported, so expect up to one extra frame of latency compared with A's independent flip. Unverified; it would need the spike.
- **Frame pacing:** the same waitable-object approach, but DWM-paced.
- **Keyboard capture:** the same low-level hook, in the Rust core or the shell.
- **UI quality ceiling:** the highest. Native Fluent controls, Mica, proper text and accessibility for free.
- **Binary size:** Windows App SDK runtime plus .NET, about 50-100 MB self-contained, or a runtime dependency. This touches the open installer question.
- **Licensing:** MIT (Windows App SDK).
- **Effort:** two languages and an FFI boundary, plus a second toolchain in CI. Rust-native WinUI (windows-app-rs, archived 2022, and the early windows-reactor work merged into windows-rs in May 2026) is not usable yet.

### C. libwebrtc (LiveKit `libwebrtc` crate) with its own decode and render

- **Transport:** Google's full stack: ICE, DTLS, SCTP, jitter buffer.
- **Latency path:** libwebrtc's video jitter buffer and playout delay add latency unless the sender signals a zero playout delay, which the host's str0m does not do. Hardware decode needs a custom D3D11 decoder factory anyway, so the decode work from A remains.
- **Frame pacing:** libwebrtc's renderer cadence, or a custom sink feeding A's presenter.
- **Keyboard capture, UI:** as in A or B.
- **Binary size:** large (prebuilt libwebrtc, tens of MB; unmeasured).
- **Licensing:** BSD/Apache, plus bundled third-party code.
- **Effort:** heavy build (prebuilt native artifact, clang, copied rustflags; MSVC steps unverified). It doesn't help WSS v4 at all.
- **Verdict:** the host is str0m, so libwebrtc brings mostly weight and a jitter buffer we would then fight.

## Recommendation

Stack A: all Rust, str0m and WSS v4 behind one transport trait, D3D11 hardware decode, a D3D11 flip-model swap chain, a low-level keyboard hook and egui on D3D11. Reasons:

- It has the shortest latency path (independent flip, present on decode, no jitter buffer).
- It shares the WebRTC crate and protocol knowledge with the host.
- It ships as one small exe.

The UI polish risk is real but bounded: the client has only three screens (connect, saved devices, session chrome), and Claude does that design work against the BubblePlay identity. If egui proves too limited, the chrome can move to Direct2D/DirectWrite on the same swap chain without touching the core.

Proposed build order once approved:

1. WSS v4 with H.264, decode and present, plus a latency overlay.
2. Input, keyboard hook and pointer capture.
3. BPA1 audio.
4. Login, approval and saved devices UI.
5. WebRTC through str0m with HEVC.
6. Monitor selection and the rest of the session chrome.

Proposed spike (throwaway, about half a day): Media Foundation H.264 and HEVC decode into D3D11 versus the D3D11 video decoding API through FFmpeg, measuring submit-to-texture and texture-to-present times on this PC with a recorded BubblePlay access-unit stream. It settles the decode route, not the stack.

## Open questions for the owner

- Approve stack A, or prefer B for the UI ceiling at the cost of size and latency?
- Decode route: Media Foundation (no extra licensing; HEVC needs the Store extension on some PCs) or FFmpeg d3d11va (LGPL DLLs, about 20 MB, HEVC everywhere)? The spike can inform this.
- Transport priority: WebRTC is direct peer-to-peer and the browser's default. WSS v4 is simpler and goes through the relay. Should v1 ship both with auto fallback like the browser, or WSS v4 only?
- Which relay does v1 target first, Go or Cloudflare? The Cloudflare site gate needs an ntfy or host approval wait screen.
- Packaging: inside the existing Windows ZIP or installer, or separate?
- Where do saved devices and the remembered session cookie live? Proposal: per-user `%APPDATA%\BubblePlay\client\`, with cookies protected by DPAPI. This brushes against auth, so it needs a decision.
