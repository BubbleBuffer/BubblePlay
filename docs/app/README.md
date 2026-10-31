# BubblePlay app (Flutter)

One Flutter app in `app/` for Windows and Android, built on [bb-ui](https://github.com/BubbleBuffer/bb-ui) v0.7.1 (git dependency, no Material). Milestone 1 is every screen except streaming, working against the real backends on both platforms. Milestone 3 adds the Windows "This PC" screen, which shows the host's Devices UI over a named pipe. Streaming is milestone 2. The egui client in `client-win/`, the old Win32 Devices window and the Android View UI are still in the repo and are removed in a later step.

## What is in milestone 1

- PC list. On Windows "This PC" is pinned above "Your PCs" and opens the Devices UI (see [This PC](#this-pc-milestone-3-windows)). Android has no such section.
- Pairing: the form (PC ID and one-time code through `CodeField`, which upper-cases and inserts the dashes, or a pairing link) and the number comparison wait with every outcome.
- Key setup, unlock, export and import. A protected imported key is stored plain, behind Windows DPAPI or the Android Keystore; a protected stored key is unlocked in memory only.
- Settings (key facts, export, theme) and the empty and error states.
- Connect opens a placeholder session: bb-ui's session chrome over a black area. No video.

Not in this version, and not offered in the UI: streaming, retrying a connect, vouching, remote pairing, key replacement.

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
- Windows "This PC" runs on a second channel of its own, the host's Devices pipe, described in the next section. It does not go through `AppChannel`.

## This PC (milestone 3, Windows)

- The host serves the Devices UI of "This PC" over a per-user named pipe. The protocol is in [devices-pipe.md](devices-pipe.md).
- `app/rust/src/devices_pipe.rs` is the client. It is exposed through flutter_rust_bridge as `devices_connect`, `devices_send` and `devices_disconnect` (`app/rust/src/api.rs`). The Dart side is `app/lib/src/devices/` (`DevicesController`), and the screen is `app/lib/src/screens/this_pc_detail.dart`.
- The pairing QR is drawn with the `qr` package into bb-ui's `QrFrame`.
- The tray's Devices item opens or focuses `bubbleplay_app.exe --section=this-pc` when the app is installed beside the tray. It also writes the stdin line `show-devices-app` to the host, which sends `show` to the connected pipe clients. Without the app installed, the old Win32 Devices window opens.

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

## Known limits

- Hosted Windows CI is not available (budget): the Windows build and the Rust tests run on the developer PC.
- Online state comes from the relay's unauthenticated `GET /presence?ids=` (max 20 ids, per-prefix rate limit, no gate hold or ntfy). `PresenceService` (`app/lib/src/presence/`) asks on the PC list and every 30 s while it is visible and no session or enrollment is open; failures keep the last value and never show online. A PC with no relay URL stays offline. The Rust and Kotlin cores still report `online: false`; Dart overlays it.
- Android fills `RemotePc.problem` from the Kotlin core's stored resource (`StateJson.pcProblem`): key changed, not accepted (relay refused or device removed), relay unreachable; anything else is null. Windows never sets a problem yet.
- Decision 2026-10-30: the old Win32 Devices window, the egui client UI and the Android View UI stay until M2 is accepted, then are removed.
- bb-ui v0.7.1 has `ObscuredField`, which `app/lib/src/screens/enroll_screen.dart` uses. The `PassphraseField` stand-in is removed.
