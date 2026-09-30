# BubblePlay for Android

A native Kotlin app (sideloaded APK, no Google Play services) that uses the home desktop through the Cloudflare Worker relay.
It uses the same WSS v4 media path and input contract as the browser receiver (`worker/public/receiver.js`).

## How it works

- **Sign-in:** a WebView opens `<server>/`, which is the unchanged Worker gate (ntfy approval, then the password page).
  - When the page navigates back to `/` with both `__Host-gate` and `__Host-bp-session` cookies set, the app intercepts that navigation. The browser receiver never loads.
  - The app then passes the cookies to the native session.
- **Signaling:** an OkHttp WebSocket to `/ws` carries hello, media-control, the first-frame handshake, desktop state, clipboard, monitors and binary input.
- **Media:** an OkHttp WebSocket to `/media-ws?epoch=…` using v4 framing.
  - The app sends a per-frame ACK (with `decodedSeq`), sends keyframe requests (limited to one per 250 ms), and sends a 15 s `ping` heartbeat.
  - Reconnect backoff runs from 250 ms up to 5 s, and the app forces a keyframe after each reconnect.
- **Decode:** MediaCodec renders to a SurfaceView.
  - HEVC is used when a hardware HEVC decoder exists, otherwise H.264.
  - `KEY_LOW_LATENCY` is requested where supported. Presentation is latest-wins.
  - On a decoder error or a sequence gap, deltas are dropped and a keyframe is requested.
- **Input:**
  - Trackpad mode: one finger moves the cursor, tap clicks, two fingers scroll, pinch zooms around the cursor, and a two-finger tap right-clicks.
  - Direct mode: tap clicks where you touch; one-finger drag pans a zoomed view; long-press then drag is a mouse drag, and long-press without moving is a right-click; two fingers scroll and pinch-zoom; a two-finger double tap resets the zoom.
  - The soft keyboard sends Unicode text for committed text and scan codes for keys.
  - Quick-action bar (icons, left to right): keyboard, switch screen (monitor list, current one marked; disabled with one monitor), touch mode, shortcuts (19 chords plus paste from the phone clipboard; no Ctrl+Alt+Del), keys (sticky Ctrl/Alt/Shift/Win, Esc, Tab, navigation, F1 to F12), arrows (D-pad with Enter, Home/End, PgUp/PgDn), zoom reset (only while zoomed), and more (full screen, settings, disconnect). Arrow and navigation keys repeat while held (350 ms, then every 40 ms). The bar hides after about 3 s; tap or swipe up the handle, or tap the status line, to bring it back.
  - Input is sent only on a signaling socket that has completed hello for the current host session and presented a first frame. If signaling drops while media is up, the app reconnects signaling with the same backoff, shows "Input reconnecting…", releases held keys and buttons, and re-arms input on the new session. A half-open socket is detected by an early ping (after 4 s of input with no inbound traffic, dead if no pong within 6 s).
  - A hardware keyboard sends scan codes.
  - A hardware mouse is captured and sends relative motion, buttons and wheel.
  - Clipboard works in both directions.
- **Session:**
  - The screen stays on and the app runs immersive.
  - A status line shows state, codec and size. Tap it to show or hide the bar, or to reconnect after an error. Full screen can be turned off from the bar's More menu (not persisted).
  - The session is stopped 30 s after the app leaves the foreground (see "Known limits").

Code layout (`app/src/main/java/de/bubblebuffer/bubbleplay/`):

- `protocol/`: v4 framing and client messages, input packets, the scan-code table. These are pure Kotlin and unit tested.
- `input/`: the touch gesture state machine, the viewport, and input sequencing and coalescing.
- `media/`: Annex-B parsing and the MediaCodec decoder.
- `net/`: server configuration and the signaling and media session.
- `ui/`: the activities, the IME target view and the cursor overlay.

## Build

Use the JDK that ships with Android Studio:

```powershell
$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
cd android
.\gradlew.bat assembleDebug testDebugUnitTest lintDebug assembleRelease
```

- `local.properties` points at the SDK and is not committed. Android Studio creates it; otherwise write `sdk.dir=C\:\\Users\\<you>\\AppData\\Local\\Android\\Sdk`.
- APKs:
  - `app/build/outputs/apk/debug/app-debug.apk`
  - `app/build/outputs/apk/release/app-release.apk`

## Release signing (never committed)

The release keystore lives outside the repo, in `%LOCALAPPDATA%\BubblePlay\android-keystore\`:

- `bubbleplay-release.jks`: PKCS12, alias `bubbleplay`.
- `keystore.properties`: holds `storeFile` (a file name relative to that folder), `storePassword`, `keyAlias` and `keyPassword`.

If `keystore.properties` is missing, `assembleRelease` produces an unsigned APK.
Back up that folder: an update signed with a different key cannot be installed over an existing install.
The `.gitignore` also excludes `*.jks`, `*.keystore` and `keystore.properties`.

## Install

```powershell
adb install -r app\build\outputs\apk\release\app-release.apk
```

Alternatively, copy the APK to the phone and open it after allowing installs from that source.

## Testing against the local Stream Lab relay (not production)

1. `cd worker && npm ci && node tools/stream-lab-direct-relay.mjs --port 8787`
2. Start a candidate host against that relay. `<compat>` is `compatibility.id` from `http://127.0.0.1:8787/lab-compat`, and `<uuid>` is any new UUID:
   `host\target\release\bubbleplay-host.exe --mode serve --worker "ws://127.0.0.1:8787/lab-host-ws?run=<uuid>&compat=<compat>"`
3. In the app, set **Server** to `http://10.0.2.2:8787/` (emulator) and **Lab run** to the same UUID, then tap Connect.
   - For a USB phone, run `adb reverse tcp:8787 tcp:8787` and use `http://127.0.0.1:8787/`.
   - Lab mode skips the sign-in gate.

Cleartext HTTP is allowed only for `10.0.2.2`, `127.0.0.1` and `localhost`. See `res/xml/network_security_config.xml`.

## Tests

`testDebugUnitTest` runs on the JVM:

- **v4 packets:** every packet in `fixtures/wss-v4-contract.json` parses (or is rejected) and re-encodes byte for byte.
- **ACK, keyframe, heartbeat and backoff output, and the receive gate:** compared with vectors generated from `worker/public/media-v4.js`. Regenerate the vectors with `node android/tools/gen-v4-vectors.mjs`.
- **Scan codes:** compared with `fixtures/input-contract.json` (scan codes, `mustBeExtended`, `vkOnly`, contract packets, invalid packets).
- **Also covered:** clipboard chunking and reassembly, the touch gesture state machine and viewport, input coalescing and text splitting, and IME text reconciliation.

## Measured (lab relay, 2026-09-30)

These runs had no input sent to the desktop. logcat shows the numbers under the `BubblePlaySession` tag, with a `stats` line every 5 s.

- **Pixel 8 Pro:**
  - The app chose `c2.exynos.hevc.decoder` (hardware, low latency on).
  - The first frame appeared 1.2 s after connect.
  - It received, decoded and presented about 25–30 fps at 1920×1080, with 0–4 skipped frames per 5 s.
- **Emulator (API 35):**
  - The app chose `c2.goldfish.hevc.decoder` (low latency off).
  - The stream played and reconnected after returning from the background.

## Known limits

- **Backgrounding:** on Android 15 (the emulator), the platform cuts the app's sockets about 1 s after it leaves the foreground. The 30 s grace period therefore only keeps the objects alive, and the app reconnects on return. Keeping the connection through a short app switch would need a foreground service.
- **Trackpad mode:** the app moves a local cursor and sends absolute positions (packet types 8 and 9), like the browser's touch mode. This avoids Windows pointer acceleration. A physical mouse sends relative motion (type 7).
- **Soft keyboard:** IMEs differ. Composing text is mirrored to the host as backspaces plus new text at the end of the text. Editing in the middle of a word on the phone is not reflected.
- **API 29 (Android 10):** layout does not adapt to the soft keyboard in immersive mode, so the bar can end up behind the keyboard.
