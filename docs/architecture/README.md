# Architecture

BubblePlay has a native Windows host, a relay that serves the browser receiver, and an optional Android receiver. The relay routes each viewer to a PC by its device ID; one session pairs one host with one viewer. Docker runs the relay and site; it does not capture or encode a Windows desktop.

## Runtime paths

| Component | Entry points | Responsibility |
| --- | --- | --- |
| Windows host | [`host/src/main.rs`](../../host/src/main.rs), [`host/src/host_capture.rs`](../../host/src/host_capture.rs), [`host/src/host_encode.rs`](../../host/src/host_encode.rs) | DXGI capture, NVIDIA encoding, session state and input |
| Per-user tray | [`host/src/bin/bubbleplay_tray.rs`](../../host/src/bin/bubbleplay_tray.rs), [`host/src/tray/`](../../host/src/tray) | Host supervision, settings, startup and rollback |
| Optional admin-input helper | [`host/src/admin_agent/`](../../host/src/admin_agent) | Validated SendInput through a restricted local pipe for elevated apps |
| Rust relay | [`relay-rs/server/`](../../relay-rs/server), [`relay-rs/core/`](../../relay-rs/core), [`relay-rs/worker/`](../../relay-rs/worker) | The only relay: site gate with ntfy approval, signaling and packet relay; native server (Docker) and Cloudflare Workers adapter (one Durable Object per PC for session sockets, a front object for the gate and the PC directory) |
| Browser viewer | [`worker/public/dk-app.js`](../../worker/public/dk-app.js), [`dk-session.js`](../../worker/public/dk-session.js), [`devicekey-store.js`](../../worker/public/devicekey-store.js) | Device-key unlock, pairing, paired-PC list and session handshake ([browser viewer](../browser-viewer.md)) |
| Browser receiver | [`worker/public/receiver.js`](../../worker/public/receiver.js), [`transport.js`](../../worker/public/transport.js), [`media-v4.js`](../../worker/public/media-v4.js) | Session lifecycle, decode/presentation, input and media feedback; assets served by the relay |
| Android receiver | [`android/app/src/main/java/de/bubblebuffer/bubbleplay/`](../../android/app/src/main/java/de/bubblebuffer/bubbleplay) | Native Kotlin WSS receiver, MediaCodec and touch input |

Development-only tools in the Rust tree are excluded from the public Windows ZIP. The admin-input helper above is the optional packaged component. Private command-execution companion packages are outside the public product.

## Transports and contracts

The `/host-ws` and `/ws` sockets carry registration and the auth and pairing handshake. After that, session signaling, desktop state, cursor/clipboard controls and input travel inside `ctl`. WebRTC carries media directly or through separately configured TURN. WSS uses `/host-media-ws` and `/media-ws`, paired by the current session epoch; files have separate `/host-file-ws` and `/file-ws` sockets. A reconnect or changed epoch invalidates old sockets and input state. [Reconnect order](reconnect.md) describes what happens when a relay reset drops every socket.

WSS v4 sends one encoded access unit per binary message, with a 28-byte header describing codec, generation, sequence, capture time and dimensions. The receiver returns frame ACKs and keyframe requests on that media socket. Generation/sequence gates prevent stale deltas from being presented after recovery or a monitor switch. Host feedback controls capture/encoding admission instead of accumulating a queue of old frames.

The browser WebCodecs receiver identifies H.264 or HEVC from Annex B keyframe NAL headers and immediately reconfigures on a codec change, decoding that same keyframe even when the WSS header still names the previous codec. WebRTC offers H.264 after HEVC when HEVC is preferred, so an H.264-only host can answer.

Shared fixtures are the cross-language contract; update them and the affected implementations together:

| Fixture | Contract |
| --- | --- |
| [`wss-v4-contract.json`](../../fixtures/wss-v4-contract.json) | Current WSS framing, ACK and keyframe messages |
| [`wss-contract.json`](../../fixtures/wss-contract.json) | Retained transport framing contracts |
| [`input-contract.json`](../../fixtures/input-contract.json) | Input packets (byte vectors for types 0 to 13) and scan codes |
| [`cursor-types.json`](../../fixtures/cursor-types.json) | Reliable semantic cursor kinds; position packets remain separate |
| [`file-transfer-contract.json`](../../fixtures/file-transfer-contract.json) | File packets, validation limits and socket behavior |
| [`follow-window-contract.json`](../../fixtures/follow-window-contract.json) | Monitor-follow capability and controls |
| [`audio-contract.json`](../../fixtures/audio-contract.json) | PC sound packets, opt-in and capability ([audio](audio.md)) |
| [`session-timing-contract.json`](../../fixtures/session-timing-contract.json) | Heartbeats, dead-detection limits, reconnect backoff and auth waits per implementation ([session timing](session-timing.md)) |

See [file transfer](file-transfer.md), [monitor following](follow-active-window.md), [session UI](session-ui.md), and [secure desktop](secure-desktop.md) for the behavior behind these contracts. [Unattended access](unattended-access.md) is approved (2026-10-13) for access before sign-in and waits for [device IDs and keys](device-keys.md), approved 2026-10-13: per-device key files, pairing, routing by ID and the ntfy site gate. The Rust relay's tests also read [browser-generated packet fixtures](../../relay-rs/core/tests/data/worker-contract.json); the Go relay that used them was deleted at CUT-1.

## Development boundaries

See [host performance](performance.md) for the event-driven producer, damage/tile path and warm reconnect behavior.

See [encoders](encoders.md) for backend selection, the shared bitstream contract, CPU mode and its diagnostic knobs.

The browser uses small modules for settings, session chrome, monitor selection, cursor translation, clipboard and files. The host separates capture adapters, encoding, session policy and telemetry from its main event loop.

Deterministic fixture tests establish packet and state contracts. They do not establish capture-driver behavior, elevated input, physical display output or a deployed relay's interoperability. Use the [development guide](../development.md) for build commands, and [release status](../release-status.md) for remaining public-release checks.
