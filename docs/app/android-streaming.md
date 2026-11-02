# Android streaming in the Flutter app

The Flutter app on Android streams the PC with the Kotlin pipeline of the native app. Video never
goes through Dart: the decoder renders into a Flutter texture and Dart only shows it.

## How it fits together

```
Dart (AppState, StreamChannel)
  | Pigeon snapshot/act/changed          | MethodChannel bubbleplay/stream
AppBackend  --StreamHost-->  StreamController  --StreamPort-->  Session (net/Session.kt)
  JSON session object          SurfaceProducer texture              VideoDecoder -> Surface
                               SurfaceHandoff                       AudioPlayer
                               StreamInput -> RemoteInput
```

- **Staging** (`app/android/app/build.gradle.kts`, task `stageNativeCore`): besides keys, auth and
  pcs, the app now compiles `net/`, `media/`, `input/` and `protocol/` from `android/` as they
  are. `input/Shortcuts.kt` is left out (it needs the native app's `R`); nothing else needed
  changing in `android/`.
- **AppBackend** (`flutter/AppBackend.kt`): `Connect` opens a session through `StreamHost` with the
  unlocked key and the device label (as `StreamActivity.startSession` does). `Disconnect`,
  `SetMode` (`setStreamMode`), `SetMonitor` (`requestMonitor`) and `SetAudio` (`setSoundMuted`) go to
  it. The `session` object of the snapshot follows the session: monitors and the selected one
  (`monitor_pending` until the host confirms, `monitor_error` on refusal), `link`
  (`Live` or `Reconnecting`), the `SecureDesktop` notice, `audio_controls_available`, and
  `stats`. A terminal session error ends the session; the reason is stored on the PC
  (`PcStore.touch`) and shown on its card. Events of an older session are ignored.
- **Stats**: `MainActivity` calls `AppBackend.refreshStats()` once a second while a session is open;
  it notifies Dart only when a number changed. `fps`, resolution, `bitrate_mbps`, `rtt_ms`, codec
  and `dropped` (decoder skipped frames) come from `Session.diagnostics`; `decode_ms` and
  `frame_ms` stay empty (the Android decoder does not time single frames). Monitors have no
  refresh rate or position on Android: they report 0 Hz and sit side by side.
- **Video** (`flutter/StreamController.kt`, `SurfaceHandoff.kt`): a `SurfaceProducer` from the
  engine's texture registry; its surface goes to `VideoDecoder.setSurface`. When the producer
  takes the surface away (`onSurfaceCleanup`), the decoder is detached (blocking, so the codec
  is released first); when a new one is there (`onSurfaceAvailable`) it is attached, and the
  decoder asks for a keyframe. The producer is resized to the stream size. `textureId` is null
  until the first live frame.
- **Input** (`flutter/StreamInput.kt`, `HidKeys.kt`): `pointer` events go through the native
  `TouchGestures` in direct mode (the finger is the pointer: tap clicks, long press then drag
  drags, long press and lift right-clicks, two fingers scroll), mapped onto the picture with the
  native `Viewport` from the area size Dart sends. `scroll` events become wheel units
  (40 logical px per notch). `key` events map the USB HID usage to the DOM code `KeyMap` uses.
  The input path is `RemoteInput`, unchanged.
- **Audio**: `Session` creates the `AudioPlayer` itself when the host streams sound.

## Not in this milestone

- No foreground service: in the background Android may stop the network, so the session can end.
  `MainActivity` tells the session when the app goes to the background (sound off, reconnect
  attempts paused) and back.
- No drawn cursor (trackpad mode), local zoom or pan, clipboard, file transfer, follow-window or
  soft keyboard text; mouse input is not told apart from touch.
- Quality has no control on Android (`quality_controls_available` is false).

## Tests

JVM unit tests under `app/android/app/src/test`: `SurfaceHandoffTest` (surface lost, available,
resize), `AppBackendStreamTest` (session actions against a fake `StreamHost`), `StreamInputTest`
(pointer and key mapping, the channel router, the HID table against `KeyMap`). Run them on
linux01 with `flutter build apk --debug` in `app`, then `./gradlew :app:testDebugUnitTest` in
`app/android`.
