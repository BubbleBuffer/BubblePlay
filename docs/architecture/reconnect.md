# Reconnect order

What happens when the relay drops every socket at once (a Cloudflare or Durable Object reset, not a network fault on the PC). Every client and the host reconnect on their own; the order they arrive in matters.

## The race

- The viewer gets `peer-left` or a close after about 0.5 s, opens a new `/ws` and is admitted by the relay.
- The host notices the loss, waits for its retry and re-registers about 1.5 to 2.5 s after the reset.
- A viewer admitted while the host is away is not known to the host. Before 2026-11-04 the relay never told the host, and a viewer `auth-hello` sent while the host was away was dropped. The viewer waited its full auth-host timeout and ended on "PC not reachable".

## Relay (`relay-rs/core`)

- When a host registers (`register_host`) and a viewer is already admitted for the PC, the host receives `peer-joined` with the viewer's `mediaEpoch` and `pairing` flag, right after `hello-ok` and before anything buffered.
- `auth-hello` is bufferable for a normal (non-pairing) viewer. It is held in the room's pending list, one latest `auth-hello` per room, within the existing pending limit. It is delivered after the `peer-joined`, and only if it was sent by the viewer socket that holds the current epoch. A hello from an older epoch, or from a viewer that has left, is never delivered: the list is cleared when a viewer is admitted and when it leaves.
- The relay still does not look at the hello's contents. The host verifies it exactly as for a live hello. If the hello arrives after the host registered it is forwarded live and not held.
- Pairing-mode viewers are not buffered.

## Host (`host/src/main.rs`)

A serve run that was registered for over 60 s and then fails reconnects at once (no 1 s sleep). Quick repeated failures still back off 1, 2, 4 ... 30 s.

## Viewers

A session that has authenticated once is "recycled" after a loss. For it, a missing `auth-host` is not terminal while about 30 s have not passed since the loss: the client opens a new signaling connection and sends a new `auth-hello`. The wait per attempt is 3 s, then 4.5 s, 6.75 s, capped at 8 s. A first connection to a PC keeps the 10 s wait and the terminal "PC not reachable". Real refusals (bad signature, unknown or changed PC key, missing cap, `ctl` error, 1008 not admitted or removed) stay terminal and are never retried. The 30 s budget restarts each time a handshake succeeds.

| Client | Where |
| --- | --- |
| Browser | [`worker/public/recovery.js`](../../worker/public/recovery.js) (`recycledAuthTimeoutMs`, `shouldRetryAuthTimeout`), `handleSessionFailure` in [`receiver.js`](../../worker/public/receiver.js), `authTimeoutMs` in [`dk-session.js`](../../worker/public/dk-session.js) |
| Windows client | `Timing::recycled_auth`, `recycled_auth_max`, `recycled_budget` in [`client-win/src/net/session.rs`](../../client-win/src/net/session.rs) |
| Android | `RecycledRetry` in `net/SessionWire.kt`, used by `net/Session.kt` |

**A reconnect never fails on the old session's messages (2026-11-11).** When a viewer reconnects signaling, the PC may still hold the previous session and send its last `ctl` messages before it has seen the new `auth-hello`; the relay forwards them to the new socket. No `ctl` key exists before `auth-host`, so the browser (`dk-session.js`) and Android (`SessionWire`) drop a `ctl` that arrives before `auth-host` is handled instead of ending the new session as `unexpected ctl`. The handshake still has its own deadline. The Windows client already skips everything but `auth-host` while it waits for it.

**Dead-socket detection on the WSS media socket.** The browser and Android ping the media socket every 15 s whatever else they send (acks and keyframe requests do not postpone the ping), and the relay answers each ping. The socket is closed and reconnected only when a ping stays unanswered for 25 s, counted from the oldest unanswered ping (`mediaHeartbeatStep` in `recovery.js`, `SignalLiveness` on Android): the same 40 s of silence as before for a genuinely dead socket. Before, only quiet-send time postponed the ping, so a static desktop (host sends about one frame a minute, viewer acks every few seconds) got no pong and healthy sockets were cut as `wss-v4-silent-socket`; and a hidden tab, whose timers run once a minute, always looked silent. The warning now also carries `unansweredMs`.

When the first frame is shown again each client logs one info line with the total recovery time and the attempt count ([logging](../logging.md)). The signaling close code is logged as `closeCode`.

## Browser: stuck socket and lost timer (2026-11-06)

- **Connect timeout.** While recovering a session that authenticated before, a signaling socket that has not opened in time is abandoned and a new one is tried: 5 s, 7.5 s, then 10 s per attempt (`connectTimeoutMs`). The old socket's handlers are detached before it is closed, so a late event cannot act on it. A first connection is not timed, since it may wait on the site gate. A reconnect is not held there: a WebSocket upgrade is allowed or refused at once (the gate holds only navigations), so the cut-off cannot interrupt an approval.
- **Signaling reconnect backoff (2026-11-12).** A lost signaling socket is retried after 250 ms, 500 ms, 1 s, 2 s, 4 s, then 5 s (`nextBackoffMsV4`, the media socket's schedule), each spread by plus or minus `V4_BACKOFF_JITTER_FRACTION` (20 %, `media-v4.js`, also applied to the media socket) so viewers that lost the relay together do not return together. The backoff restarts once a session authenticates.
- **Offer failures (2026-11-12).** A failed offer restarts the peer up to `OFFER_FAILURE_LIMIT` (3, `recovery.js`) times in a row; a sent offer resets the count. After that auto mode switches to WSS and a page forced to `transport=webrtc` shows `offer-failed`.
- **Decode backlog (2026-11-12).** WSS video is decoded frame by frame; a decoder with `V4_DECODE_BACKLOG_LIMIT_FRAMES` (30, `media-v4.js`) frames queued counts as wedged: the viewer drops to the next keyframe and requests one (main-thread decode and `wss-pipeline.js`). The host keeps at most 12 frames in flight.
- **A control message the handler rejects (2026-11-12).** An authentic, in-order `ctl` message whose handler throws is logged (`control message dropped`) and dropped. Failures to open, order or authenticate a message still end the session.
- **Wake.** While a reconnect is pending (its backoff timer is set), `visibilitychange` to visible, `pageshow` and `online` run it at once (`shouldRetryOnWake`), unless a socket is already connecting or the session ended. A frozen or throttled page could otherwise leave the retry timer unfired for minutes. There is no polling timer.
- Real refusals stay terminal. The abandon and wake lines are logged ([logging](../logging.md)).
- The Windows client bounds the `/ws` open by its 10 s handshake limit.

## Android: stuck upgrade (2026-11-06)

- The signaling socket (`openSignaling` in `net/Session.kt`) uses `forSignaling()` from [`net/SignalingHttp.kt`](../../android/app/src/main/java/de/bubblebuffer/bubbleplay/net/SignalingHttp.kt): an OkHttp `callTimeout` of 10 s (`SIGNALING_HANDSHAKE_MS`) over the whole open, so DNS, connect, TLS and the upgrade share one limit, like the Windows client. Before, only a 10 s connect timeout applied and a socket that connected but never answered the upgrade waited forever.
- A timeout is an ordinary failure with no HTTP response (`onFailure`), so the existing signaling retry and the recycled-session budget above handle it. It is not a terminal refusal.
- OkHttp stops the call timeout once the upgrade succeeds (`timeoutEarlyExit`) and clears the socket's read timeout, so an established quiet socket is never cut. `SignalingHttpTest` checks both against real sockets: a server that accepts and never answers fails inside the limit, and an upgraded socket stays open past it. The media socket and `PairingClient` are unchanged.
- The Flutter app (`app/`) compiles this `net/` package from the native app's tree (`stageNativeCore` in `app/android/app/build.gradle.kts`), so both Android apps share the fix.

## Android phone app: background (2026-11-09)

The Flutter app has no foreground service, wake lock or Wi-Fi lock (the foreground service `SessionService` belongs to the native app's `ui/` package, which the phone app does not compile). Before this change, going to the background only paused reconnect attempts and muted the sound: the media socket, the decoder, the audio track (volume 0, still decoding), the host's encoder and a 250 ms session tick all kept running.

`BackgroundGrace` ([`net/BackgroundGrace.kt`](../../android/app/src/main/java/de/bubblebuffer/bubbleplay/net/BackgroundGrace.kt)) is the state machine; `Session` owns it only when constructed with `backgroundGraceMs` (the phone app passes 5 minutes; the native app does not, so it is unchanged). `MainActivity.onStop` and `onStart` drive it through `Session.setForeground`.

- **Backgrounded** (the activity stops; screen off counts): media is closed at once, as for an ordinary media loss (`closeMedia`: media socket, first-frame and retry timers, audio), and the decoder releases its codec (`VideoDecoder.idle`; the surface stays). Media stays closed until the app returns (`mediaPaused`), also when a surface is attached meanwhile, and the surface watchdog does not count misses. The host sees its media peer leave and stops capture and encoding; the signaling socket, its 15 s ping and the reconnect hold stay as they were. The session tick slows to 1 s; the app's stats timer stops.
- **Back within 5 minutes:** media reopens through the media-only path above (same signaling epoch and grant, `media-control start`, first-frame request), with no new authentication or pairing prompt. If signaling dropped meanwhile, the held reconnect runs first.
- **After 5 minutes:** the session reports `onBackgroundExpired` and `AppBackend` ends it exactly as Disconnect does (`stream.close`, no session, PC list). Nothing reconnects by itself: the phone app has no auto-connect, so the next open is the normal one (the user taps the PC). The clock is `elapsedRealtime`: the timer runs on uptime, which stops in deep sleep, so a return after the grace disconnects even if the timer never fired.
- A Disconnect or a terminal error during the grace cancels the timer.
- Logs ([logging](../logging.md)): one info line each for backgrounded, resumed within grace (with `firstFrameMs`) and grace expired.
- Host side (checked in code, not on a device): a media-socket close with signaling up stops video capture and sends nothing as an error. Host audio capture and Opus encoding keep running until the viewer's signaling ends, and each resume costs one IDR; there is no restart limit.

## Not covered

The media session does not survive a signaling failure: a signaling reset still rebuilds the media link. Keeping it alive needs a protocol change and is a separate decision.
