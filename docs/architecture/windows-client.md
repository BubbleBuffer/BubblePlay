# Native Windows client

Status: stack A approved 2026-10-13. Decode route: Media Foundation (spike 2026-10-14). The native shell and WSS v4 development path are implemented in `client-win/`.

## Current state

Step 6 adds live monitor selection and Work/Game controls over `media-control` messages on both WSS and WebRTC, sent as `ctl` kinds on the authenticated control channel. Monitor selection exposes pending and error state to the picker, and WebRTC keeps its peer and track through a switch, reopening admission only after the new generation's admitted RTP timestamp. Live quality and audio actions are unavailable; the mock keeps them available. Follow is not offered.

The first monitor list applies `RemotePc::default_monitor` once per session, and a user pick updates that field in memory. Persistence remains a known gap until the saved-devices store is implemented; step 6 introduces no storage format.

The loopback-only `--dev-headless --dev-session-script` check uses recorded H.264, real decode and synthetic input to switch displays and then Game → Work. The opt-in `session::headless::step6_e2e` tests use `BUBBLEPLAY_CLIENT_EXE` and `BUBBLEPLAY_FAKE_HOST_EXE` for the built binaries; the fake's `--monitors` enables its two-display list and `--monitor-error SELECTOR` scripts a failure. The harness budgets cold startup separately from the client's live clock, keeps the host alive through client shutdown, and records child wall times. It requires one signaling epoch and, on RTC, one peer with no fallback. These checks cover decoded admission; window presentation and physical capture require separate qualification.

Follow-up qualification on 2026-10-16 passed three consecutive serial RTC runs, decoding 957, 1640 and 1725 frames with the monitor switch, admission reopening and Game → Work completed in epoch 1 on one peer. An earlier trial failed to regain Work input readiness amid substantial RTP loss and assembly resets; its serial rerun began the passing streak, without changing assertions. The WSS rerun exposed fake-host accounting that omitted frames deliberately retired by a monitor switch. That report correction and its two regressions remain uncompiled: another lane held the shared build lock through the remaining budget. Fake-host tests/Clippy/build, rebuilt-host WSS qualification and client Clippy (default and `dev-login`) remain outstanding; the owned waiters were stopped and their exact queue entries removed.

Documentation conflict for the main session: `docs/architecture/follow-active-window.md:3` and `docs/architecture/session-ui.md:51` disagree about Android Follow.

Step 1 implements the two-socket WSS v4 handshake, epoch validation, BPV4 parsing, ACK-before-gate, keyframe requests, heartbeats and reconnect. Media Foundation decodes NV12 textures on the shell's shared D3D11 device; the latest-frame mailbox wakes the shell, and a video processor crops, scales and letterboxes before egui. The existing H.264/HEVC recorded-stream baselines and recovery checks remain step-1 evidence, not live qualification of this input change.

Step 2 adds `src/input/` and integrates the read-only shell's `InputCapture` contract at commit `7e87a45`. A new session defaults to Work (browser `pointerMode: "desktop"`, wire `streamMode: "work"`): absolute positions and buttons map through the displayed letterbox; keyboard input starts after video focus; the remote cursor kind selects a local system cursor. Game uses foreground Raw Input, hides/clips the pointer to the picture, and installs a dedicated keyboard-hook thread while capture, readiness and foreground eligibility hold. Its callback uses atomics and a bounded queue; the UI drains keys. Session code exposes `CaptureStatus`/`SessionInputState` for later chrome. No new UI is drawn.

Ctrl+Alt+Shift+Z releases or recaptures; Ctrl+Alt+Shift+Q leaves. Fresh chord modifiers and terminal keys are reserved. Registered hotkeys retain the escape route while capture is released. Focus loss, release, readiness loss and teardown clear held input and remove capture. Input is binary on `/ws`, gated by the first admitted presentation and the desktop enabled/available state. Shared u16 motion/absolute/button sequences wrap and reset with each epoch. Cursor packets 128/129 update state; exact matching type-128 acknowledgements can measure absolute-input round trips in Work.

Run these commands from `client-win/`, after building both crates through the shared FIFO heavy-build lock and verified F: target junction:

```powershell
.\target\debug\fake-wss-host.exe --streams-dir F:\build\spike-streams --port 19764 --exit-after 35 --input-record F:\build\input.jsonl --stats F:\build\fake-host.json
.\target\debug\bubbleplay-client.exe --dev-wss ws://127.0.0.1:19764 --mode work --codec h264 --exit-after 30 --stats F:\build\native-client.json
```

Use `--mode game` for Game; omit the flag for Work. Starting Game engages the hook and pointer capture: run desktop qualification only on VM 101, never on the owner's working PC. The fake host records decoded valid/refused packets, held state and disconnect lifecycle in JSON lines, and sends a finite scripted cursor sequence. Its `--seq-gap-at 5 --generation-at 10 --socket-drop-at 15` offsets start at host launch. The normal session now requests audio with `audio:1` (step 3); the input-only fixture keeps its decode-free path. Automatic codec selection uses H.264 for forced WSS and tries HEVC before H.264 for RTC when the decoder supports it; explicit HEVC remains available on `--dev-wss` with an installed hardware decoder.

### Device key and PC list (phase 2, stage A, 2026-10-25)

The password, TOTP and cookie login is gone: `--password-file`, `--totp-provision`, the `dev-login` feature, `net/login.rs` and `net/cookies.rs` were deleted, and no socket sends an `Origin` header or cookies. The relay admits the viewer by its device key ([device keys](device-keys.md), "Phase 2 relay protocol"). Only the dev CLI path (`--dev-relay`, `--dev-wss`, `net/` and `session/`) uses the key; the egui screens still run against the mock backend.

- **Key at rest:** `%LOCALAPPDATA%\BubblePlay\viewer\device-key.bpkey`, the portable `.bpkey` JSON (`crates/bp-device-key` `KeyFile`) wrapped in a CurrentUser DPAPI blob (`src/store/key.rs`, `src/store/dpapi.rs`). A key with a passphrase asks for it on the console, without echo, when a session opens it. A key that cannot be unlocked means no connection.
- **`--key <file.bpkey>`** reads a plain (not DPAPI-wrapped) key file instead, for development.
- **PC list:** `%LOCALAPPDATA%\BubblePlay\viewer\pcs.json`, a JSON array of `{name, id, public, relay}` (`public` is the pinned PC key, base64url, empty until paired). `--pair` pins a PC there on `pair-done` (`PcStore::pin`); a listed PC with a different key is refused, never replaced.
- **Subcommands** (the executable is a GUI program, so pipe it, for example `... | Out-Host`, to keep the shell from reading the console at the same time):

```powershell
.\target\debug\bubbleplay-client.exe key create [--label L] [--passphrase-prompt]
.\target\debug\bubbleplay-client.exe key import C:\path\viewer.bpkey
.\target\debug\bubbleplay-client.exe key export C:\path\viewer.bpkey   # plain portable file; never overwrites
.\target\debug\bubbleplay-client.exe key show                            # prints the viewer ID
.\target\debug\bubbleplay-client.exe pcs list
.\target\debug\bubbleplay-client.exe pcs remove <id>
```

- **Relay session:** `--dev-relay <origin> --pc <id>`. The origin is `https://host[:port]`, or `http://` for a loopback relay only. `--pc` is required here and optional on `--dev-wss`. Since stage B the PC must be paired (a pinned key in `pcs.json`); an unpaired PC is refused with a pointer to `--pair`.
- **Pairing:** `--pair <code> --pc <id> --dev-relay <origin>` pairs and exits (see "Device-key session (phase 2, stage B)" below).

```powershell
.\target\release\bubbleplay-client.exe --dev-relay https://relay.example --pc 77VH-7M56-8GJK-Y12J --codec auto --mode work --exit-after 30 --stats-out F:\build\relay-stats.json
```

On the wire: `/ws?pc=<id>`, then `{"type":"hello","role":"viewer","id","public"}`. The relay answers `register-challenge {nonce}`, and the client replies `{"type":"register-proof","signature"}`, signed with `sign_relay_proof(Role::Viewer, origin, nonce)` where the origin is `relay_origin` of the relay URL. `hello-ok` carries `mediaEpoch`, `iceServers` and a `ticket`. The media socket is `/media-ws?pc=<id>&epoch=<epoch>`, with hello `{"type":"hello","role":"media-viewer","ticket"}`. A 1008 close on `/ws`, before or after `hello-ok`, is fatal (`NetError::NotAdmitted`) and is not retried. A `hello-ok` without a ticket is a protocol error outside pairing. `mediaEpoch` is an opaque 32-lowercase-hex string (decision 22): it is checked, then used byte for byte in the media URL and the handshake transcript, never parsed as a number.

### Device-key session (phase 2, stage B, 2026-10-27)

All cryptography is `bp_device_key::phase2` (feature `phase2`); `src/net/phase2.rs` is only glue: message shapes, checks and the state machines (`ViewerHandshake`, `Ctl`, `Pairing`, `Binder`).

**Failures (rulings 2026-10-28):**
- (a) Auth and crypto failures end the session with no automatic retry: a bad signature or MAC, a `ctl` seq gap or repeat, a refused cap, an unknown or changed PC key, a `ctl` error from the PC (`NetError::DeviceKeys`), and a 1008 close on `/ws` before or after the handshake (`NetError::NotAdmitted`). A plain network drop, or any other relay close before or after the handshake, follows the normal reconnect path, and every reconnect runs a fresh handshake: new nonce and ephemeral key, `ctl` seq back to 0.
- (b) Pairing times out after 300 s.
- (c) A pairing `hello-ok` that carries a ticket is refused.
- (d) An unknown PC is refused at the handshake, naming its ID and the fix: "pair with --pair <code> or the pairing link".

1. After `hello-ok` the viewer sends plaintext `auth-hello {id, public, nonce, ephemeral, caps}` with caps `control-aead-v1`, `dtls-bind-v1` (plus `pair-v1` when pairing). No `auth-host` within 10 s ends the session with "This PC does not support device keys or is not reachable".
2. `auth-host` is checked against the pinned key for `--pc` (unknown PC: ruling (d); other key: "PC key changed"), the transcript hash `H` covers relay origin, epoch, both IDs, keys, nonces and ephemerals. The viewer answers `auth-finish` and switches `/ws` to `ctl`.
3. The viewer's first `ctl` is `{type:"caps", caps, name}` with the computer name cut to 64 characters (decision 19). The PC's first `ctl` must be `caps`; WebRTC without `dtls-bind-v1` is refused. A `ctl` `{type:"error", code:"dtls-bind-required"|"cap-missing", message}` ends the session with a clear message (decision 21). Nothing (offer, `media-control` start) is sent before this.
4. **Routing on `/ws` after the handshake.** Sealed in `ctl` (kind = the message `type`), outbound: `caps`, `media-control` (mode, start, keyframe, stop, monitor), `first-frame`, `offer`, `ice`, and binary input as kind `input`. Inbound: `caps`, `error`, `answer`, `ice`, `first-frame`, `desktop-state`, `monitor-list`, `monitor-error`, `input-capabilities`, and binary `cursor` frames (the old binary cursor frame as plaintext, decision 20; binary kinds skip the type check). Plaintext stays: relay messages (`hello-ok`, `register-challenge`, `error`, `peer-left`, `peer-joined`, `pair-closed`, `ping`, `pong`) and `auth-*`, `pair-*`. Any other plaintext or a bare binary frame after the handshake is dropped and counted; a `ctl` with a bad seq, tag or kind ends the session.
5. **DTLS bind (WebRTC).** After the answer the viewer sends plaintext `auth-bind` over `H` and both DTLS fingerprints, and verifies the PC's. RTP media and input over the peer are accepted only once both binds verify. `/media-ws` (WSS) is unaffected. Fingerprints (decision 23): each comes from the `a=fingerprint` line of the SDP text actually sent (local, the offer) or received (remote, the answer), never from str0m's API; only the `sha-256` line is used when several are listed; both are normalised to `sha-256 AB:CD:...` before signing and verifying by `bp_device_key::phase2::sdp_sha256_fingerprint` and `normalize_fingerprint`; neither `client-win` nor `tools/fake-wss-host` parses fingerprints itself. An SDP without a sha-256 fingerprint ends the session with no retry; `tools/fake-wss-host` answers such an offer with `ctl` error `dtls-bind-required` and closes.

**Pairing** (`--pair <code> --pc <id> --dev-relay <origin>`): the code is normalised, the hello carries `pair` = the admit hash, and `hello-ok` must say `"pairing":true` (a ticket there is refused). After the handshake the viewer sends its `pair-proof` first; the number (`482 913`) and "Confirm on the PC" are printed only after the PC's proof verifies. `pair-done` stores `{name, id, public, relay}` in `pcs.json`; `pair-denied`, a wrong proof or a timeout (300 s) store nothing and exit non-zero.

Test PC: `tools/fake-wss-host` and the in-crate fake relays play the PC with the fixture "PC test" key from `fixtures/device-keys-contract.json` (ID `77VH-7M56-8GJK-Y12J`).

End-to-end against the real Rust relay: `src/session/relay_e2e_tests.rs` starts `bp-relay-server` in-process (a dev-dependency) with the PC side in `tests/support/relay_pc.rs`, and runs the real client threads. It covers an admitted connect with `ctl` both ways and a media ticket, a pinned-key mismatch refused after `auth-host`, `--pair` pinning the PC in `pcs.json` followed by a normal connect, and an unlisted viewer closed with 1008 (fatal). These run in the crate's lib test binary.

### Log and feedback upload hook (2026-11-02)

`net::CtlLogHook` (`client-win/src/net/ctl_hook.rs`, held in `NetConfig::log_hook`) is the core's generic side of [client-log-upload.md](client-log-upload.md); the Flutter app's upload queue uses it through the frb layer. With a hook configured the session advertises `client-logs-v1` in the `auth-hello` and the ctl caps; without one nothing changes.

- Events: `Opened{pc_id, peer_caps}` after each authenticated connection (the hook sends and delivers only if the PC advertised the cap too), `Message(json)` for the host's `logs-state`, `logs-ack` and `feedback-ack` (every other kind keeps its normal handling), `Closed` when the socket epoch ends. A reconnect is a new `Opened`; queued messages die with their epoch.
- `send(json)` queues `logs-batch`, `feedback-begin` or `feedback-chunk` (64 messages, 1 MiB, 128 KiB each; false when full, not connected or another kind). The session writes at most one per loop turn, last in `tick`, and none in a turn that wrote or still holds mode, monitor, input or keyframe work. They are sealed as ctl kinds like other endpoint messages.
- `buffered_bytes()` is the hook queue (the WebSocket layer exposes no socket queue size); `since_last_input()` is stamped where `send_input_packet` writes (WebRTC lanes and the binary ctl `input` path), `Duration::MAX` before the first.

### CI

The `client-win` job in `.github/workflows/checks.yml` runs on windows-latest with the pinned 1.97.1 toolchain:

- `cargo test --locked` for the crate, which includes the `tests/ui_layout.rs` snapshots;
- all-target Clippy with `-D warnings`;
- `rustfmt --check` from `src/main.rs`. The frozen mock UI (`src/ui`, `src/mock.rs`, `src/model.rs`) is `#[rustfmt::skip]` in `main.rs`, and `tests/ui_layout.rs` is not covered.

`--stats-out` contains received/decoded/presented rates over the final one-second window, frame intervals p50/p95/p99, receipt-to-new-frame-blit p50/p95, requests/drops/errors, codec/resolution, and Work absolute-input RTT when a matching cursor ACK exists. Presentation timing uses CPU successful video-processor blit callbacks, not scanout. Video quantiles retain the most recent 8192 samples. Input RTT runs from type-8 socket send start to receipt of an ACK_VALID type-128 with the same sequence and epoch; it includes host cursor publication and return transport, excludes UI queueing/display, and remains null without matching echoes. Input quantiles retain the most recent 512 matching echoes in the current epoch; the sample counter is cumulative within that epoch.

Step-2 validation on 2026-10-14: default client suite 170 passed (3 ignored, VM-only), `--features dev-login` 183 passed (3 ignored), `ui_layout` 10 passed, fake-host 92 passed. A default release binary rejects `--dev-relay` as an unknown flag; the dev-login binary refuses a non-loopback origin before reading files. On VM 101 (interactive scheduled task, decode-free window), Work and Game end-to-end runs passed twice in a row: mode-correct packet types (Work 8/9, Game 7 with Raw Input deltas), monotonic sequences reset per epoch, release-all on the release chord, focus loss and reconnect after a socket drop with nothing held at exit, nothing sent while released, quit chord unforwarded and ending the session before the timed exit, Win and Alt+Tab forwarded by the Game hook, and the hook still forwarding keys during and after a 1.4 s UI stall (registry LowLevelHooksTimeout absent, 1 s assumed). The VM runs do not exercise the real shell window, video decode or WARP rendering. Desktop-free unit suites cover fixture bytes/refusals, scan/E0/browser VK mapping, sequences, letterbox edges, chords/state/readiness, cursor ACK matching, login vectors/URL restrictions/single POST/HTTP cookies and captured-log/stats secret checks. VM-only tests are ignored by default and require `BUBBLEPLAY_INPUT_LIVE_TESTS=1` plus `BUBBLEPLAY_INPUT_TEST_DIR`. `--dev-input-only` uses an explicitly opted-in, decode-free native test window with the same capture and WSS paths, so VM checks do not depend on hardware MF decode or the shell's D3D11 initialization.

Limits: clipboard UI and device keys remain pending. Ctrl+wheel is reserved locally like the browser; viewport zoom itself remains UI work. Ctrl+Alt+Del and Win+L remain Windows security shortcuts. A simultaneous drop of both sockets cannot guarantee an outgoing release-all packet; the recorder distinguishes actual packets from host disconnect cleanup, and reconnect starts with a release barrier. Work Win/Alt+Tab are not injected in the automated check because local Windows navigation can change foreground midway through a chord; Work's absence of hooks/clipping is checked. Decode-free VM runs qualify input plumbing, not video rendering, WARP shell behavior or real-host interoperability. Colour conversion remains limited-range BT.709. Live quality controls remain unavailable; step 6 supplies monitor selection.


Step 3 adds `src/audio/`: strict fixture-backed BPA1 parsing, libopus (`opus` 0.4.0) stereo decode and a bounded packet jitter buffer, behind an `AudioSink` trait. BPA1 is demultiplexed before BPV4; it has no ACK, keyframe or input-readiness side effects. A normal WSS start includes `audio:1` after the audio decoder is created. Socket epochs, host departure/rejoin and reconnect flush ingress, decoder and output; video gaps, video generations and desktop gating leave audio running. `SessionStats.audio` exposes `SessionAudioControl` (`set_volume`, `set_muted`, `stats`) for later UI wiring. Each session starts at volume 1 and unmuted. Development flags `--audio-volume 0..1` and `--audio-mute` exercise these local controls without drawing UI.

The initial lead is **40 ms**, matching the browser. Audio depth (software plus device padding) is capped at **150 ms**; excess discards old audio, keeps the newest **60 ms** sequence window and flushes old decoded/device PCM. Frames arriving after their deadline are dropped, and every emitted PCM sample is refused if its projected device time is more than 150 ms after local socket receipt. The native reorder window and PLC extend the browser's gap handling: it waits for missing packets until their playout deadline, uses libopus PLC for at most **three 10 ms frames**, then goes silent and rebases instead of generating audio indefinitely while the host suppresses silence. The host disables FEC, so `decode_fec` is false. A discontinuity flushes/rebases immediately. Sequence ordering wraps at u32; sustained sender/device drift is handled by bounded drops/rebases rather than growing latency. There is no additional A/V synchronization.

WASAPI runs on an owned COM/MMCSS render thread in shared event mode. It first requests the minimum `IAudioClient3` engine period, then tries the endpoint mix format with local format conversion/resampling. Unsupported channel layouts or padded integer valid-bit formats use standard shared PCM conversion and its default device period. Endpoint failures go silent and retry no more than once per second; the default endpoint is checked once per second. Drop joins the render thread. No real endpoint is opened by the fake sink.

Headless audio qualification creates no window, D3D renderer or input capture. Build the client and fake host through the shared FIFO lock, then run from `client-win/`:

```powershell
.\target\debug\fake-wss-host.exe --audio --audio-loss-every 37 --audio-reorder-every 23 --port 19764 --exit-after 23 --stats F:\build\audio-host.json
.\target\debug\bubbleplay-client.exe --dev-wss ws://127.0.0.1:19764 --dev-audio-only --exit-after 20 --stats-out F:\build\audio-client.json
```

The fake host needs no video recordings for this run. Its 440 Hz stereo tone uses the host's 48 kHz Opus settings (960 samples per channel on the WSS path), no FEC/DTX, with deterministic wire loss/reorder; wire loss leaves an unflagged sequence gap for PLC. Real host-side resets/queue drops still mark discontinuity. `--dev-audio-only` defaults to the fake sink and accepts only a fake host on 127.0.0.1. `--dev-audio-wasapi` additionally selects real output and requires `BUBBLEPLAY_AUDIO_DEVICE_TESTS=1`; use only on the test VM. On that VM, `--dev-audio-render-test --exit-after 3 --stats-out <path>` generates a local tone through the same audio path without a host or window, under the same environment opt-in.

Both default and `dev-login` builds accept `--stats-out` on `--dev-wss`. Its `audio` section records received, rejected, decoded, concealed and dropped packets, late/duplicate counts, depth p50/p95/max, missing-frame underruns, hardware-empty observations, device errors/reopens/period/buffer, and post-volume PCM RMS/frequency. Quantiles retain 2048 recent samples; totals and maxima are cumulative. A decoded packet with a discarded PCM tail also counts as dropped. Receipt-to-output measures local receipt to scheduled PCM (including reported padding), not host-to-ear latency or audible quality. Missing-frame underruns and empty hardware padding are separate counters. Final snapshots follow joined worker shutdown.

Missing `ID3D11VideoDevice`/required video context interfaces, matching driver profiles or suitable decoder MFTs produce typed `NoHardwareVideoDecoder` errors. The session exits nonzero with one clear line and writes the requested stats file. `--dev-decoder-probe --codec h264 --stats-out <path>` checks this path without creating a window or connecting to a host.

Step-3 validation on 2026-10-14: default / `dev-login` client unit suites **210 / 223 passed, 4 ignored each** (three VM-only input tests plus the opt-in external audio run); all **28 audio tests** pass. Fake-host suite **122 passed**, with no ignored tests; its all-target Clippy run is clean. The opt-in headless audio run passes separately for 20 seconds: **1927 decoded**, **53 scripted losses**, **84 reordered**, **56 concealed** (53 missing packets plus three bounded PLC frames after the host stops), **0 dropped**, and no decode/device errors. PCM measures **440.00 Hz**, RMS **0.1752**. Depth p50/p95 is **40.437/50.437 ms**, maximum **65.437 ms**; maximum receipt-to-scheduled-output is **77.407 ms**. The fake host also served recorded video; all 1188 video frames were ACKed without opening a window or decoding video, and audio generated no ACKs/keyframe requests. Audio JSON and test logs are in `F:/build/win-client-step3/`.

VM 101: the copied default executable (`SHA256 8607c8ac...ebc6cd85`) ran headless in a one-off interactive task. The WASAPI path attempts three opens during a 3 s run and reports **no default playback endpoint**, `0x80070490`; real rendering, underruns and the device period are **not measured**. No endpoint or driver was installed. The decoder probe exits **1**, writes stats with `error_kind: no_hardware_video_decoder`, and emits exactly one nonempty stderr line: `BubblePlay: no hardware video decoder (H.264): D3D11 device has no ID3D11VideoDevice interface`. Normal session startup was also tested on VM 101 against a copied local fake host: it exits **1** with one stderr line, `BubblePlay: no hardware video decoder (H.264): the shell adapter has no usable D3D11/NV12 decoder`, and writes typed failure stats. This covers the actual shell-device startup path as well as the headless capability probe. Each one-off task is unregistered, its owned processes have exited, and keepawake is removed. Physical playback, device changes and real-host interoperability remain unqualified.

Build/Clippy acceptance: default and `dev-login` client builds, and the fake-host build, pass without warnings. The fake-host all-target Clippy run passes with `-D warnings`. Both client all-target Clippy variants (default and `dev-login`) pass with `-D warnings` as of step 4.

Step 4 (2026-10-14) adds the login, approval and saved-devices UI against the mock backend; there is still no network code behind it. Connecting uses device-keys pairing plus the before-sign-in approval wait. There is no sign-in, password or TOTP screen, and the Go dev login stays CLI-only. A first start without a key shows Set up (create a key, or import a key file, with a local passphrase unlock or a refusal). Pairing has a code path and a remote-request path, and each ends approved, denied, timed out or closed at the PC. Approval can end denied, timed out or withdrawn. Connect failures are typed (`model::ConnError`): no first frame, relay unreachable, PC busy, connection lost, no hardware video decoder, PC key changed. Your PCs covers the empty list, offline PCs, per-PC problems (`RemotePc::problem`), inline rename and a forget confirmation. A new PC opens in Work, with no mode choice before connecting. The seam the real device-keys backend implements is documented at the top of `src/model.rs`. `--ui-demo` has 50 states (the approval-withdrawn scene, 47, was added after step 5). `tests/ui_layout.rs` checks the step-4 scenes and the pairing screens at 1100x720 and at the 760x520 minimum (`NN-name-min.png`).

Step 5 (2026-10-14) adds `src/net/webrtc/` using str0m **0.21.0**, with a client-owned UDP socket on the existing dedicated network thread. Signaling uses the same authenticated `/ws` session. The receive offer has video followed by stereo Opus and the DCEP channels `input-control` (ordered, reliable) and `input-motion` (unordered, zero retransmits). HEVC precedes H.264 when the decoder supports HEVC; H.264 remains available. The browser JavaScript does not fix SDP payload IDs, DCEP IDs or fmtp strings, so str0m supplies those RFC parameters; source-contract tests check the browser's media directions, labels and channel parameters rather than claiming a captured Chromium SDP match.

H.264 single NAL/STAP-A/FU-A and HEVC single NAL/AP/FU packets assemble into bounded Annex-B access units, with loss, reorder, sequence-wrap and malformed-input handling. Units feed the existing MF decoder. Decoder recovery and damaged units request RTCP PLI. Opus retains extended RTP sequence and timestamp through the step-3 decoder and jitter buffer. Raw production input packets retain their bytes: types 7 and 8 use the motion channel, including wheel-only motion packets, and all discrete packets use the control channel. Cursor echo packets supply the same absolute-input RTT measurements as WSS.

The dev CLI selects `--transport auto|webrtc|wss` (default auto). Auto remembers a terminal RTC failure within the session and opens WSS v4 through the existing media epoch; a surviving peer and its input channels remain usable after media switches to WSS. Timing follows `worker/public/receiver.js:332-353` (first-frame watchdog **5 s**, one retry), `worker/public/media.js:767-785,815-865` (connected ICE and DTLS, usable stats, zero video RTP, **5 s since both the latest request and startup progress**) and `worker/public/receiver.js:1676-1686,2005-2009` (terminal RTC failure switches auto to WSS). The watchdog's second expiry alone does not prove an RTC transport failure; a peer receiving RTP keeps the browser's behavior.

**Deliberate browser deviation:** `AUTO_ICE_CHECKING_TIMEOUT` is a single named **10 s** default in `src/net/config.rs`, measured from successful offer transmission. The browser keeps waiting while ICE checks (`worker/public/media.js:824`); the native auto path switches to WSS at this deadline. `--ice-timeout-ms <1..60000>` overrides it for development. Every fallback in `--stats-out` has a stable reason and `offer_to_fallback_ms`: `ice-checking-timeout`, `no-rtp`, `rtc-failed` before decoded video, or `mid-session-drop` after video. The first-frame retry uses the browser's `first-frame-timeout` signaling reason and null RTC epoch; it is separate from the eventual fallback reason.

Headless qualification uses `--dev-headless`, a real D3D11/MF decoder, no window or presentation, and the fake audio sink. `--dev-input-script work|game` sends synthetic production-encoded packets after actual decode readiness; it does not capture, hook or inject OS input. `--dev-count-only` explicitly selects a decode-free counter run. A typed no-hardware-decoder preflight can produce a counter-only report with failed decode qualification; a decoder failure after startup remains an explicit failure. These flags accept only loopback endpoints. Run the two commands in separate terminals after locked builds:

```powershell
F:\build\targets\fake-wss-host\debug\fake-wss-host.exe --webrtc --audio --streams-dir F:\build\spike-streams --port 19764 --exit-after 35 --input-record F:\build\rtc-input.jsonl --stats F:\build\rtc-host.json
.\target\debug\bubbleplay-client.exe --dev-wss ws://127.0.0.1:19764 --dev-headless --transport auto --codec auto --dev-input-script work --exit-after 20 --stats-out F:\build\rtc-client.json
```

Fake-host knobs `--block-ice`, `--stall-first-frame` and `--rtc-drop-at <seconds>` exercise checking, connected-without-video and real DTLS close-notify failure paths. Input JSON lines identify `input-control`, `input-motion` or `ws`. The host stops RTP when WSS media starts while retaining usable channels.

The `webrtc` stats section records ICE transitions, selected candidate pair type, RTT, packet loss, NACK/PLI/FIR counts, jitter, audio counters, input RTT and video rates/quantiles. Headless presentation and receive-to-present values are null; actual decoder callbacks supply receive-to-decode and decoded frame intervals. Shared video/audio/input measurements are labeled as whole-session measurements, including WSS after fallback; RTP counters describe RTC only. Candidate handling supports the active IPv4 interface and server-reflexive candidates; loopback qualification refuses public ICE servers before DNS. TURN relay allocation, real-host/relay interoperability, physical presentation and real audio output are not qualified by these runs.

Step-5 checks on 2026-10-14: default and `dev-login` client suites **286 / 300 passed, four opt-in tests ignored each**; the default suite includes **62 WebRTC tests**. Fake-host suite **146 passed**; its updated established-second Opus cadence assertion also passes separately. Default and `dev-login` all-target client Clippy, all-target fake-host Clippy, and both development executable builds pass with zero warnings.

Nine 20-second headless cases passed before the final fake-audio pacing correction: forced RTC H.264 **1,142** decoded / **1,207** Opus; automatic RTC HEVC **1,115 / 1,250**; checking timeout **555 / 907** at **10,031.9 ms**; 2,000 ms checking override **990 / 1,496** at **2,026.9 ms**; no RTP **570 / 1,428** at **10,024.8 ms**; early terminal failure **1,137 / 1,919** at **547.5 ms**; mid-session drop **1,150 / 1,525** at **10,980.6 ms**; synthetic Game **1,047 / 1,173**; forced WSS **1,162 / 1,783**. Every passing run stays on one signaling session, records input on the correct channels, passes the 440 Hz tone check, and uses actual MF output with null presentation. Nominal H.264 / HEVC receipt-to-decode p50/p95/p99 is **3.752/4.973/8.914 ms** / **4.092/5.587/12.560 ms**. This is local decode evidence, with both executable hashes pinned per run.

The first too-short fake-host run and a cold checking run failed qualification and remain preserved: cold RTC setup plus roughly four seconds of MF decoder creation can consume the remaining part of a 20-second rescue run. The checking deadline still fired at 10,038.7 ms and WSS stayed on the same session; the identical case subsequently passed without client changes. Let the fake host outlive preflight, the 20-second session and joined cleanup; the example gives it 35 seconds.

Counter review found that skipping late fake RTC audio ticks underpaced the 48 kHz clock (about 73 ten-millisecond packets per second) and caused substantial concealment/late drops. The fake sender now retains its 100 Hz audio cadence; full tests, the focused cadence check, Clippy and its build pass. Qualification of that final sender binary is still pending: two attempts had long RTP gaps and no input readiness while Windows reported **100% system CPU after all owned test processes exited**, independently confirmed with `GetSystemTimes`. No unrelated processes were stopped. Earlier passing results use the pre-correction fake binary and must not be represented as a final-sender qualification. Artifacts, earlier failures, SHA-256 identities and counters are in `F:/build/win-client-step5/`.

Final-sender qualification on 2026-10-15 (commit 27cd03f, release builds) ran once under foreign load (CPU 52-92% per case) and is a **known gap**, not an acceptance: 5 of 9 cases passed (`rtc-h264-paced`, `rtc-game-paced`, `wss-paced` on its one rerun, `auto-ice-override-paced`, `auto-rtc-failed-paced`). The failures had MF decoder creation of 4.7-17 s (23 ms in a passing run): `auto-hevc-paced` did not exit in 35 s; `auto-ice-checking-paced` and `auto-no-rtp-paced` fell back correctly (10,011 / 10,014 ms) but decoded only one frame; `auto-mid-drop-paced` reported `rtc-failed` at 11,964 ms instead of `mid-session-drop`, which needs a quiet-machine rerun to tell a load effect from a classification bug. Outputs and hashes are in `F:/build/win-client-step5/qual-20261015/`. A rerun on an uncontended machine is still owed.

Backlog: MF software H.264 decode fallback for adapters without `ID3D11VideoDevice` (VMs, RDP, unusual drivers), after the v1 core steps. The current client deliberately has no software decode fallback.

## Goal

A native Windows viewer that feels clearly better than the browser receiver: low latency and smooth frame pacing, hardware decode, full keyboard capture (Win, Alt+Tab, Ctrl+Esc go to the remote PC while focused, with an obvious escape chord), proper pointer capture, a fast polished UI (connect screen, saved devices, in-session chrome) and audio.

v1 scope: device-key pairing and the approval wait, video, mouse/keyboard/scroll input, audio, monitor selection, and session chrome matching [session UI](session-ui.md). File transfer, clipboard and the rest come after v1.

Constraint: no host, relay or protocol changes. The client speaks exactly what the browser receiver speaks.

## What the client must speak

Learned from `worker/public/`, `relay/` and `worker/src/` (both relays deleted at CUT-1) and `host/src/` (research notes 2026-10-13; line references are in those files, not repeated here).

- **Admission (phase 2, replaces the old relays' password and cookie login, deleted at CUT-1; the Rust relay's ntfy site gate comes first).** A signed viewer hello on `/ws?pc=<id>`, then a register-challenge and register-proof. See "Device key and PC list" above and [device keys](device-keys.md).
- **Signaling.** `wss://<relay>/ws?pc=<id>`. The reply `hello-ok` carries `iceServers`, a fresh `mediaEpoch` and the media `ticket`. Every media socket and `media-control` message must carry that epoch. Reconnect is a new hello and a new epoch.
- **Two media transports, both already served by the host:**
  - **WebRTC** (browser default in auto mode): the client offers recvonly HEVC + H.264 video and Opus audio, and opens two data channels, `input-control` (ordered, reliable) and `input-motion` (unordered, no retransmits). The host is str0m 0.21, prefers HEVC, gathers host and server-reflexive candidates only (no TURN client on the host side).
  - **WSS v4**: `/media-ws?pc=<id>&epoch=E`, hello `media-viewer` with the ticket. One Annex-B access unit per binary message behind a 28-byte `BPV4` header; the client ACKs every frame and asks for keyframes on gaps. Audio rides the same socket as BPA1 packets (one 20 ms Opus frame each; WebRTC uses 10 ms) when `start` carries `audio:1`. Input goes as binary on `/ws`. Traffic passes through the relay.
- **Input.** Little-endian binary packets from `fixtures/input-contract.json`: set-1 scan codes with an E0 flag (type 13), relative motion with wheel (7), absolute position (8), buttons-at (9), release-all (6), with one wrapping u16 sequence per epoch. Cursor state and cursor kind come back as packets 128 and 129.
- **Monitor switch.** `media-control` `monitor` (a `ctl` kind). The browser's in-place switch needs `requestVideoFrameCallback` RTP timestamps; a native client can match the admitted `rtpTimestamp` itself on WebRTC, or restart in a new epoch as the browser's fallback does.

Nothing in the protocol depends on a browser.

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

## Recommendation (approved 2026-10-13)

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

## Decisions

All dated 2026-10-13.

- **Transports.** v1 ships both. WSS v4 is built first, then WebRTC with auto fallback, matching the browser.
- **Relays.** The Go and Cloudflare relays were deleted at CUT-1. The [Rust relay](../../relay-rs/README.md), with the approval wait screen, is required before v1 is called done.
- **Packaging.** For v1 the client is a separate portable exe. Installer integration is decided later.
- **Local storage** (superseded 2026-10-25 by phase 2 device keys). The viewer key and the PC list live in `%LOCALAPPDATA%\BubblePlay\viewer\`, with the key wrapped by DPAPI. There are no cookies.
- **Codecs.** H.264 is first-class, because hosts with Media Foundation encoders produce only H.264 and the host will follow codec changes on keyframes. The client must work on a PC without the HEVC extension, by checking at startup which decoders are installed.
- **Decode route: Media Foundation** (2026-10-14, from the spike in [`client-win/spikes/decode-latency`](../../client-win/spikes/decode-latency/README.md)). Median submit-to-presented is within 0.3 ms of FFmpeg d3d11va for both codecs on the GTX 1660 Super (H.264 about 4.5 ms, HEVC about 3.3 ms, present-on-decode). MF H.264 has much tighter tails (p99 about 11 ms against 54 ms). Both backends show HEVC stalls of 100-500 ms. The machine was under heavy memory and build load during the runs, so the tails are not reliable; a quiet-machine rerun is still owed. The MF HEVC MFT here has no ICodecAPI; low latency is set through the `MF_LOW_LATENCY` attribute instead. FFmpeg stays out of the client.
- **Step 6 scope** (2026-10-15, main session). Rule for every case: copy what the browser does today, with no new protocol messages. Monitor switch uses `media-control` `monitor` on signaling with one pending switch, as `monitor-selection.js` does. On WebRTC the client keeps the peer and switches in place, admitting frames by the host's `rtpTimestamp`; a new epoch is used only if that cannot map onto str0m, with the reason recorded here. Work/Game changes live with `media-control` `mode`, as the browser does. Quality and audio controls come later: on the live path they are disabled, not hidden, so the approved layout stays. The Follow row stays out. The saved default monitor per PC is applied once per session.

## UI layout tests

`client-win/tests/ui_layout.rs` renders every `--ui-demo` state (50, from
`src/mock.rs`) with egui_kittest, at 1100x720 and at the 760x520 shell
minimum, and checks the accesskit tree. The UI marks its containers and
texts through `src/ui/contract.rs` (`box:`, `row:`, `scroll:` names; a text
class with its baseline and whether it may truncate). Invariants:

- every item lies inside its container, and siblings in a container do not
  overlap (scroll bars excepted);
- nothing lies outside the window, after clipping to scroll viewports;
- no text is elided unless it was marked as allowed to truncate;
- buttons are at least 32 px high;
- texts in one row share a baseline;
- no run-on line: more than three texts on one line with gaps under 16 px;
- egui's "First use / Second use of ... ID" warning is not painted.

Run on Windows (it needs a wgpu adapter): `cargo test --test ui_layout` in
`client-win/`. A failure lists each problem as `[state @size] what: item`.

Snapshots: `snapshots_at_default_size` compares each state at 1100x720 with
`client-win/tests/snapshots/<state>.png` (threshold 0.8, at most 400 differing
pixels); `snapshots_at_minimum_size` does the same at 760x520 for the
pairing screens and the step-4 states, as `<state>-min.png`. Snapshots leave
the demo scene label out of the title bar. After an intended visual change, regenerate them with
`UPDATE_SNAPSHOTS=1 cargo test --test ui_layout`, look at the new PNGs and
commit them with the change. The mockup comparison behind the current layout
is in `client-win/src/ui/REFINEMENT.md`.

## Open questions

- None open. Stack A (all Rust, egui on D3D11) was approved 2026-10-13.
