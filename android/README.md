# BubblePlay for Android

A native Kotlin app (sideloaded APK, no Google Play services) that uses the home desktop through the Cloudflare Worker relay.
It uses the same WSS v4 media path and input contract as the browser receiver (`worker/public/receiver.js`).

Start with the [public documentation](../docs/README.md) for host/relay setup and [architecture](../docs/architecture/README.md) for the shared contracts. Android's packaged/device acceptance is separate from the Windows/browser alpha. Interoperability with the new Go relay has not been demonstrated.

On a fresh install, enter your own relay address before connecting. Existing saved addresses are retained; the app has no built-in personal relay destination.

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
  - Quick-action bar (icons, left to right): keyboard, switch screen (monitor list, current one marked; disabled with one monitor), touch mode, shortcuts (19 chords plus paste from the phone clipboard; no Ctrl+Alt+Del), keys (sticky Ctrl/Alt/Shift/Win, Esc, Tab, navigation, F1 to F12), arrows (D-pad with Enter, Home/End, PgUp/PgDn), zoom reset (only while zoomed), and more (full screen, settings, disconnect). When the host advertises `followWindow`, the switch-screen popover adds a "Follow active window" row below the monitors (On/Off; the host's `monitor-list.follow` wins over the saved choice). Arrow and navigation keys repeat while held (350 ms, then every 40 ms). The bar is always visible and docked: the stream is laid out (letterbox and touch mapping included) in the area above it, and with the soft keyboard open the bar sits on the keyboard with the stream above. Back closes the settings view, then a popover, then the keyboard, then asks to disconnect.
  - Input is sent only on a signaling socket that has completed hello for the current host session and presented a first frame. If signaling drops while media is up, the app reconnects signaling with the same backoff, shows "Input reconnecting…", releases held keys and buttons, and re-arms input on the new session. A half-open socket is detected by an early ping (after 4 s of input with no inbound traffic, dead if no pong within 6 s).
  - A hardware keyboard sends scan codes. With **Capture keyboard shortcuts** on (default), Esc goes to the PC and keys that arrive with Win held (Android reports `META_ON` even when it keeps the Win key itself) are wrapped in a synthetic Win press, so Win+R, Win+E or Win+Shift+S reach the PC. Android consumes Win alone, Alt+Tab and Print Screen before any app sees them; Shortcuts in the bar covers those. With it off, Esc releases a captured mouse and Win combinations are sent only as Android delivers them.
  - A hardware mouse follows the **Pointer** setting, like the browser's pointer modes:
    - **Work** (default): the mouse points where it is on the stream (absolute positions, packet types 8 and 9) and the Android pointer shows the host's cursor shape. The stream runs in the host's Work mode (30 fps).
    - **Game**: the first click on the stream captures the mouse (Android pointer capture) and is not sent; then motion is relative (type 7) with buttons and wheel. The stream runs in the host's Game mode (60 fps). Esc releases the capture (with keyboard capture on: hold Esc for 1 s; a tap still goes to the PC). A touch, focus loss, opening settings or switching to Work also release it. A notice says how to release.
  - Clipboard works in both directions.
- **Session:**
  - The screen stays on and the app runs immersive (both are settings).
  - A status line shows state, codec and size. Tap it to reconnect after an error. Full screen and touch mode can also be switched from the bar for the current session only.
  - Foreground windows that need Admin mode show “This window needs Admin mode — turn it on at the PC” in that status line. An unavailable Admin mode helper shows “Admin mode helper isn’t responding”. The warning clears when the host clears its reason, and controls stay enabled. Optional `input-status` reason and route fields are validated; older receipt-only messages preserve the current warning.
  - **Background:** with **Stay connected in the background** on (default), a foreground service (`ui/SessionService.kt`, type `dataSync`) starts with the session and stops with it. While it runs, the session, file transfers and reconnects continue when the app leaves the foreground, and an ongoing low-priority notification ("Connected to <host>") returns to the stream on tap and has a Disconnect action. Android 13+ asks for the notification permission at the first session start. The service only starts while the app is in the foreground. When Android refuses it, ends it (Android 15+: 6 h of `dataSync` per 24 h), or the setting is off, the session is stopped 30 s after the app leaves the foreground (see "Known limits").
  - **Resume:** when the app pauses, it copies the current video frame (`PixelCopy`). On return that frame stays on screen, dimmed, with a "Reconnecting…" label, until the first live frame is presented; it then fades out over 150 ms. If the copy failed, the screen is black until the stream returns. The frame is kept across the 30 s background stop and dropped on Disconnect or Sign out.
  - **Reconnect in the background:** without the background service, lost signaling or media sockets are not reconnected while the app is in the background. On return (`onStart`) the backoff resets and any held or waiting retry runs at once.
  - **Surface watchdog:** the 2 s steps (forced keyframe, media restart, give up) count only while signaling is up, and restart from the next `hello-ok`, so a slow host reconnect does not end in "Stream recovery stopped".
  - logcat (`BubblePlayStream`) logs `foreground to first live frame N ms` on each return.

Code layout (`app/src/main/java/de/bubblebuffer/bubbleplay/`):

- `protocol/`: v4 framing and client messages, input packets, the scan-code table. These are pure Kotlin and unit tested.
- `input/`: the touch gesture state machine, the viewport, and input sequencing and coalescing.
- `media/`: Annex-B parsing and the MediaCodec decoder.
- `net/`: server configuration and the signaling and media session.
- `files/`: file transfer v1 (flow control, names, the transfer engine, MediaStore downloads).
- `ui/`: the activities, the IME target view and the cursor overlay.

### Settings

See the shared [session UI](../docs/architecture/session-ui.md). One view (`ui/SettingsView.kt`), opened from More in the bar and from the connection screen. From the bar it opens over the stream inside `StreamActivity`, so the stream stays in the foreground; Disconnect stays a separate item in More. Values persist in `Prefs` (SharedPreferences, schema version 2) and apply live:

- **Connection:** server address (saved here applies to the next connection), Sign out (expires the `__Host-gate` and `__Host-bp-session` cookies for that server, ends the session, returns to the connection screen), stay connected in the background (default on; turning it off in a session stops the service now).
- **Touch:** default touch mode, trackpad pointer speed (levels 1 to 9; 5 is the old fixed gain 1.6, each step 25 %), tap to click (trackpad one-finger tap; when off, long-press clicks and a two-finger tap still right-clicks).
- **Mouse and keyboard:** pointer mode for a hardware mouse (Work or Game, default Work; a change in a session switches the host's stream mode now), capture keyboard shortcuts (default on).
- **Display:** start in full screen, keep screen on, default monitor (requested once when a session starts, if the host lists it and is not already showing it; choosing one in a session switches to it now).
- **Video:** codec (Auto, HEVC, H.264). A change reconnects the stream with a short notice; transfers in flight end with the old session.
- **Diagnostics:** decoder, resolution and codec, presented frames/s and media bitrate (1 s windows), relay round trip (signaling ping to pong; the view sends an extra ping every 5 s while open). Copy report puts these plus app, device and settings (server host only, no cookies) on the clipboard.
- **About:** app version.

### File transfer (v1)

See [file-transfer behavior](../docs/architecture/file-transfer.md) and the [shared fixture](../fixtures/file-transfer-contract.json). All of it is hidden unless the host's `host-capabilities` carries `fileTransfer` >= 1.

- **Socket:** a second OkHttp WebSocket, `/file-ws?epoch=<mediaEpoch>` (lab: `/lab-file-ws` with `run` and `compat`), with the same gate cookie as `/ws`. It opens on demand, sends `hello`, waits for `file-peer` present (10 s), pings every 15 s and closes after 60 s idle. One transfer runs per direction; the rest queue.
- **Upload:**
  - Share sheet: "BubblePlay" accepts `SEND`/`SEND_MULTIPLE` of any type, purpose `save`. Without a session it opens the app (connect or sign in first), then uploads. At most 64 files go; the rest are reported.
  - The Attach icon in the quick bar opens the system picker (multiple).
  - Shortcuts → Paste: a content URI on the phone clipboard is uploaded with purpose `paste` instead of pasting text.
  - Files are read through `ContentResolver`; no storage permission. A source without a known size is copied to the app cache first.
- **Download:** a `file-offer` shows a chip above the bar. Download requests each item, writes it to `Download/BubblePlay` via MediaStore (pending until the SHA-256 and size check out, deleted otherwise), then shows a notification and an in-app notice with Open / Share. Notification permission is asked on the first download (Android 13+). A withdrawn offer removes the chip.
- **Progress:** a slim row per direction above the bar (name, percent, cancel). Cancel stops the active transfer and its queue.
- **Lifetime:** transfers belong to the stream session and continue in the background while the background service runs. Ending the session (disconnect, or the 30 s background stop without the service) cancels them all. If the host leaves or the epoch changes, the file socket drops: the transfer in flight fails, and queued ones retry on a new socket (up to 3 failed opens, 10 s wait for the host's file socket). Partial downloads are always discarded.

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

Each build's `versionCode` is the build time in minutes since the Unix epoch, so every new build outranks the installed one and update stores (F-Droid repos, Obtainium) offer it. `versionName` is `0.1.0-<that number>`. Pass `-PversionCode=<n>` to set it explicitly.

## Install

```powershell
adb install -r app\build\outputs\apk\release\app-release.apk
```

Alternatively, copy the APK to the phone and open it after allowing installs from that source.

## Testing against the local Stream Lab relay (not production)

1. Install dependencies with `npm --prefix worker ci` from the repository root, then start `node worker/tools/lab/stream-lab-direct-relay.mjs --port 8787`.
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
- **Cursor appearance:** the separate two-byte type-129 packet and Android system pointer mappings match `fixtures/cursor-types.json`; unknown kinds fall back to the arrow. Type-128 position packets keep their eight-byte format. Touch/trackpad and captured mice draw semantic overlay glyphs; uncaptured mice use Android system pointers. Android maps Progress to Wait and UpArrow to Arrow; the overlay distinguishes them. Only explicit Hidden suppresses the trackpad cursor, preserving older hosts' visibility behavior.
- **Settings:** `Prefs` defaults, version 1 to 2 migration (codec spellings normalized, last touch mode kept as the default), malformed values; pointer-speed mapping; tap-to-click off; pointer mode, follow window, keyboard capture and background defaults and round trips.
- **Parity features:** the follow-window capability, control message and `monitor-list.follow` against `fixtures/follow-window-contract.json`; the swallowed capture click (`CaptureClick`); the synthetic Win press (`MetaSync`); the background service state (`KeepAlive`: refusal or timeout falls back once, a requested stop is not an end, Disconnect from the notification).
- **File transfer:** every packet, rejected case, receiver session and signaling case in `fixtures/file-transfer-contract.json`; the send window and ACK pacing; size and hash mismatch; MediaStore names; share-intent parsing.
- **Resume:** the reconnect loop (backoff reset on start, no attempts while in the background), the surface watchdog not counting while signaling is down, and the frozen-frame overlay state (shown on return with a snapshot, cleared on the first presented frame, dropped on disconnect).
- **Also covered:** clipboard chunking and reassembly, the touch gesture state machine and viewport, input coalescing and text splitting, and IME text reconciliation.

## Historical lab measurements (2026-09-30)

These earlier runs describe those devices and builds; they do not establish acceptance of the current APK or Go relay. No input was sent to the desktop. logcat shows the numbers under the `BubblePlaySession` tag, with a `stats` line every 5 s.

- **Pixel 8 Pro:**
  - The app chose `c2.exynos.hevc.decoder` (hardware, low latency on).
  - The first frame appeared 1.2 s after connect.
  - It received, decoded and presented about 25–30 fps at 1920×1080, with 0–4 skipped frames per 5 s.
- **Emulator (API 35):**
  - The app chose `c2.goldfish.hevc.decoder` (low latency off).
  - The stream played and reconnected after returning from the background.

## Known limits

- **Backgrounding:** without the background service, Android 15 (the emulator) cuts the app's sockets about 1 s after it leaves the foreground. The 30 s grace period then only keeps the objects alive, and the app reconnects on return.
- **Background service:** the app holds no wake lock (it would need a new permission), so with the screen off Doze can still stall the sockets; the app reconnects when it wakes or returns. Video keeps arriving while in the background; frames without a surface are dropped and a keyframe is forced on return. Android 15+ ends `dataSync` services after 6 h per 24 h.
- **Keyboard capture:** Android has no public API to capture Win alone, Alt+Tab or Print Screen (`OVERRIDE_SYSTEM_KEY_BEHAVIOR_IN_FOCUSED_WINDOW` is signature/privileged); pointer capture does not change key routing.
- **Trackpad mode:** the app moves a local cursor and sends absolute positions (packet types 8 and 9), like the browser's touch mode. This avoids Windows pointer acceleration. A physical mouse sends relative motion (type 7).
- **Soft keyboard:** IMEs differ. Composing text is mirrored to the host as backspaces plus new text at the end of the text. Editing in the middle of a word on the phone is not reflected.
- **API 29 (Android 10):** layout does not adapt to the soft keyboard in immersive mode, so the bar can end up behind the keyboard.
