# BubblePlay app (Flutter)

One Flutter app in `app/` for Windows and Android, built on [bb-ui](https://github.com/BubbleBuffer/bb-ui) v0.7.0 (git dependency, no Material). Milestone 1 is every screen except streaming, working against the real backends on both platforms. Streaming is milestone 2, the host Devices UI over a named pipe is milestone 3. The egui client in `client-win/` keeps building until milestone 3.

## What is in milestone 1

- PC list. On Windows "This PC" is pinned above "Your PCs"; it is a stub that reads "Not connected" until the host UI lands (M3). Android has no such section.
- Pairing: the form (PC ID and one-time code through `CodeField`, which upper-cases and inserts the dashes, or a pairing link) and the number comparison wait with every outcome.
- Key setup, unlock, export and import. A protected imported key is stored plain, behind Windows DPAPI or the Android Keystore; a protected stored key is unlocked in memory only.
- Settings (key facts, export, theme) and the empty and error states.
- Connect opens a session (placeholder in M1, real streaming since M2, see Streaming).

Not in this version, and not offered in the UI: streaming, retrying a connect, vouching, remote pairing, key replacement, the host Devices window, online presence of PCs.

## Architecture

Flutter owns every screen. The core behind it differs per platform but speaks one JSON contract:

```
Dart screens ──► AppController ──► AppChannel ──► Windows: flutter_rust_bridge ──► Rust AppBackend (app/rust)
                                              └─► Android: Pigeon ───────────────► Kotlin AppBackend (app/android)
```

- `AppChannel` (`app/lib/src/backend/channel.dart`) has three calls and a change stream: `snapshot()`, `act(actionJson)`, `exportKey()`, `changes`. `FrbChannel` is Windows, `PigeonChannel` Android, `FakeChannel` is for tests and goldens.
- The snapshot is the JSON of `AppState`, an action the JSON of `Action`. The Rust serde types in `client-win/src/model.rs` are the source of truth; `app/lib/src/model/` mirrors them by hand and `app/test/fixtures/contract/{states,actions}.json` is generated from the Rust side (`UPDATE_FIXTURES=1 cargo test --test contract_fixtures` in `app/rust`). Dart round-trips every fixture (`test/contract`), Kotlin checks that its JSON builders have the same fields (`StateJsonTest`), so the two cores cannot drift apart without a test failing.
- Windows: `client-win` is now a library (`bubbleplay_client`) plus the egui binary behind the default-on `egui-client` cargo feature. `app/rust` links the library without that feature and implements the `Backend` trait for real (`backend.rs`): key and PC list under `%LOCALAPPDATA%\BubblePlay\viewer`, pairing through `net::pair`. The Dart bindings in `app/lib/src/rust` are generated (`flutter_rust_bridge_codegen generate` in `app/`).
- Android: the Kotlin core (keys, auth, pcs, pairing client) stays in `android/app/src/main/java`; the Flutter module compiles those packages through a Gradle source set (`app/android/app/build.gradle.kts`) and adds `AppBackend.kt`, `StateJson.kt` and the Pigeon host API (`MainActivity.kt`). Actions run on one worker thread, so key derivation never blocks the UI. The Pigeon files are generated from `app/pigeons/backend.dart`.

## Screens and tests

Screens are in `app/lib/src/screens/`. Each takes the snapshot as an argument and sends actions through `AppScope.of(context).controller`. The app id on Android is `de.bubblebuffer.bubbleplay.flutter`, distinct from the existing native app.

Tests in `app/test/`:

- `contract/` — fixture round trips.
- `screens/*_test.dart` — widget tests at desktop (1000 px) and phone (390 px) that assert what is shown and which actions are sent.
- `screens/*_golden_test.dart` (tag `golden`) — pictures per screen at both widths, dark and light. They are rendered on Linux CI (build01), never on a PC; to refresh them run the App workflow with "Update goldens" and commit the artifact.

## Build and run

- Tests: `cd app; flutter test` (`--exclude-tags golden` off CI).
- Android debug APK and Kotlin tests: the App workflow (`.github/workflows/app.yml`, runs on build01).
- Windows: `cd app; flutter build windows`. `app/build` is a junction to `F:\build\targets\BubblePlay-app\build` (output stays off C:). Cargokit builds the Rust core in the CMake step; the Rust target goes to the same F: area. The Rust FFI tests (`cargo test` in `app/rust`) need Windows because the key store uses DPAPI.


## Streaming (milestone 2)

Connect now streams the PC. Video never passes through Dart or the bridge: the platform decodes and renders into a native texture, Flutter shows it with a `Texture` widget.

```
Dart StreamVideo ──► StreamChannel (MethodChannel "bubbleplay/stream": textureId, videoRect, pointer, key)
   Windows: runner stream_plugin.cc ──► bubbleplay_app_core.dll (bp_stream_* C ABI) ──► client-win/src/stream StreamSession
   Android: MainActivity ──► SurfaceProducer texture ──► Kotlin Session + VideoDecoder   (docs/app/android-streaming.md)
```

- Windows: `StreamSession` (`client-win/src/stream/player.rs`, ungated, shared with the egui client's decode, ordering, stats and input modules) owns its own D3D11 device and an owner thread that decodes (Media Foundation) and draws each new frame with `VideoRenderer` into one of two alternating shared BGRA textures (`target.rs`), then `Flush`es. The runner registers a `GpuSurfaceTexture` (DXGI shared handle) whose descriptor comes from `bp_stream_front`, and marks a frame available from the core's frame callback. The two textures alternate because the engine does no synchronisation with the producer.
- Windows input is captured natively: the runner's `FlutterWindow::MessageHandler` offers every window message to `bp_stream_message` first (raw input, keyboard hook, cursor clip), and a timer drives the capture's per-frame step. Dart reports the video rectangle in physical pixels; `pointer` and `key` are ignored on Windows. Android forwards touch and physical keys from Dart.
- `app/rust/src/stream_host.rs` holds the one running session. `AppBackend::connect` starts it, `Disconnect` drops it, the snapshot overlays its monitors, mode, link and stats, and a session that ends on an error becomes `Flow::Failed` or `Flow::Refused` plus a `PcProblem` on the PC. State-change notifications are throttled to about four a second.
- Tests: `client-win/tests/stream_target.rs` (a frame drawn on the session device is readable through the shared handle on a second device), `app/rust/tests/backend.rs`, `app/test/screens/stream_video_test.dart` (texture, rectangle, Android pointer and key forwarding), the Kotlin tests listed in `android-streaming.md`.

### Measured latency

Windows, headless against `fake-wss-host` (1080p60 H.264 recording, 20 s, dev profile, software H.264 MFT because this PC has no hardware decoder; `client-win/tests/stream_latency.rs`, run with `--ignored --nocapture`): 60 fps, 1203 of 1205 frames presented, decode to present p50 0.28 ms / p95 0.42 ms, publish to readable on a second D3D11 device p50 0.4 ms / p95 0.6 ms / max 1.0 ms. That is far below one frame at 60 Hz (16.7 ms), so the Flutter texture path is kept; no fallback is needed. Not measured: the egui path (it cannot run without a visible window) and the engine's own compositing; the on-screen check needs the owner. Android SurfaceProducer versus SurfaceView is not measured yet (needs the owner's phone); the SurfaceView fallback is not built.

### Building on Windows

`app/windows/toolchain-sdk22621.cmake` pins Windows SDK 10.0.22621 (SDK 10.0.26100 lacks x64 gdi32.lib here, MSB8037). Set `CMAKE_TOOLCHAIN_FILE` to it for the first configure. If the build fails on a long path, map the worktree to a short drive (`subst W: <worktree>`) and build from `W:\app`. Keep Cargo output on F: (`CARGO_TARGET_DIR=F:\build\targets\flutter-app-core`).

## Known limits

- Hosted Windows CI is not available (budget): the Windows build and the Rust tests run on the developer PC.
- Android runs the existing Kotlin core as is; presence (`online`) and per-PC problems are not asked yet on either platform.
- bb-ui has no obscured text field; `app/lib/src/widgets/passphrase_field.dart` is the stand-in and a candidate for a bb-ui addition.
