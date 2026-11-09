# Logging, crash capture and feedback

Every BubblePlay process writes JSON-lines logs. Redaction runs before anything is written. All logs are meant to end up on the PC (client logs arrive over the session channel once that upload is built; see "Status").

## Line format

One JSON object per line, UTF-8, at most 8 KiB:

```json
{"ts":"2026-11-02T13:45:12.345Z","level":"info","src":"host","device":null,"pc":null,"session":null,"target":"module::path","msg":"text","fields":{}}
```

- `ts`: UTC RFC 3339 with milliseconds.
- `level`: trace, debug, info, warn, error. Files are written at info and above.
- `src`: host, tray, agent, helper, relay, app-win, app-android, app-android-core, browser.
- `device`: the writer's own device ID. `pc`: the PC the line relates to. `session`: session ID.
- `fields.error` and `fields.stack` carry errors. Panics and uncaught errors are lines with level error and target `panic` (Rust) or `uncaught` (Dart, Kotlin, browser).

## Redaction

Applied to `msg` and every string in `fields`, and re-applied by the host on ingest:

1. Values of fields whose name contains password, passphrase, secret, token, key, proof, ticket, code, pairing, psk, signature, auth, cookie or credential (allow-list: keyboard, keyframe, keycode, keyid, and a few close variants).
2. URL parameters with those names; `Bearer` tokens; Authorization headers.
3. `name=value` pairs in free text with those names.
4. Literal secrets a process registered at runtime.
5. Any run of 40 or more characters from `[A-Za-z0-9+/_=-]` outside the `device`, `pc` and `session` fields.

Consequence for authors: do not name log fields `code`, `key`, `auth` and the like (use `status`, `hresult`, `reason`); the value is masked. The one exception is `closeCode` (the WebSocket close code, a number such as 1006 or 1001), which is kept in the host, browser and Android redactors so the two can be told apart. `authCloseCode` and every other `code` key stay masked.

## Where files live

On the PC, under `%LOCALAPPDATA%\BubblePlay\logs` (host, tray) or the protected install `logs` folder (admin agent, secure helper). Override with `BUBBLEPLAY_LOG_DIR`.

| Files | Contents | Retention |
|---|---|---|
| `host-YYYY-MM-DD.jsonl`, `tray-...`, `agent-...`, `helper-...` | the PC's own processes (`.N.jsonl` after 20 MiB) | 7 days, 20 files per source |
| `clients\<device-id>\YYYY-MM-DD.jsonl`, `device.json`, `cursor.json` | logs sent by paired clients | 14 days, 20 MiB per device, 200 MiB total (swept at host start and every 24 h) |
| `feedback\<YYYYMMDDTHHMMSSmmmZ>-<device-id>.json` (+ `.png`) | feedback reports | kept |

`device.json` (name as paired, platform from the stream, app version, last seen) is written when a device's first batch on a connection arrives and its `last_seen` moves with every batch. Feedback reports are never swept. The host's own logs are age-cleaned (7 days) with a 20-file cap per source on top, which only cuts below 7 days after more than about three 20 MiB rollovers a day.

The tray log holds only the host's and agent's output that is not already in their own logs, as lines with target `child`: a panic message, a backtrace, native library output, a startup failure before the child's log was open. Their tracing lines are never echoed.

Rates: the host's per-second statistics (WSS producer window and link totals as one `WSS stats` line, `remote input diagnostics`, `secure input summary`) go to debug and so not to the files; info carries their sum at most once per 60 s, plus one at session end. The monitor previews add one `monitor previews: session summary` info line per viewer session that negotiated the capability (`sent`, `bytes`, `skipped_unchanged`, `skipped_gated`, `skipped_budget`, `capture_failures`, `max_capture_ms`; see [architecture/monitor-previews.md](architecture/monitor-previews.md)). Encoder retunes (`NVENC pending configuration applied`) are info only when the encoder is reset. Host lines written while a viewer session is active carry its relay epoch (the 32-character session epoch the host already holds) in `session`; lines between sessions have `session: null`.

On clients: `logs/pc/<pc-id>/<src>-YYYY-MM-DD.jsonl` (lines tied to a PC, 5 MB per PC), `logs/misc/` (the rest), `logs/outbox/` (reports waiting to be sent). 20 MB total, 30 days. The Rust side of the Windows app writes `app-win-rust-YYYY-MM-DD.jsonl` (src `app-win`, 60 lines per minute) into the same tree. Roots: Windows app `%LOCALAPPDATA%\BubblePlay\viewer\logs`; Android the app's `files/logs`, shared by the Dart process (`app-android`) and the Kotlin core (`app-android-core`), each writing its own files. The browser keeps a 2 MB ring in IndexedDB `bp-logs` (`lines`, `feedback`).

The relay logs structured JSON to the console (Workers observability); `pc` and peer IDs are cut to 8 characters, and every refusal has a class (for example `pairing.wrong-code`) and a 1008 close reason clients can show.

## Feedback

"Report a problem" is in the Flutter app (Ctrl+Shift+F, Settings, session menu), the browser viewer (Ctrl+Shift+F, More, Settings) and the tray menu (opens the app's sheet over the devices pipe, `open-feedback` / `openFeedback`). A report holds the text, kind (bug, complaint, idea, other), timestamp, app version and commit, platform, device, PC, session, screen, the last 200 log lines, the last 20 errors and, in the app, a screenshot of the Flutter UI (never the stream). On Android, shaking the phone opens the sheet (not during a session). If the app is not installed, the tray shows a small text box and saves the report straight to `feedback\<ts>-this-pc.json`. It is saved in the client outbox and sent to the PC the client is connected to.

## Warn versus info (updated 2026-11-03)

Expected events log at info: media closed by the relay with "viewer departed" or a stale epoch, mDNS `.local` remote candidates, input paused for a native file drag, capture unavailable because the display is off, and the tray's agent probe finding no pipe within 20 s of starting the agent. Anything else in those places stays at warn. Serve failures log the time since the last relay registration; the serve retry delay restarts at 1 s after a run that was registered for over 60 s. The WSS v4 media link pings every 15 s on its own clock, and a silent-link close reports the inbound age and ping count. The tray logs why it stops the admin agent (`reason`).

## Finding what happened around a feedback timestamp

```
bubbleplay-logs feedback
bubbleplay-logs around 2026-11-02T13:45:12Z --window 5m [--src host,tray] [--device <id>]
```

`feedback` lists reports with time and first line. `around` merges the PC's own logs and all client logs in the window, sorted by `ts`, each line prefixed by its file.

## Upload to the PC

A connected, authenticated client sends its logs and reports to the PC over the sealed session control channel, with the capability `client-logs-v1`. The wire format, cursors, acks and priority rules are in [architecture/client-log-upload.md](architecture/client-log-upload.md); the vectors are in `fixtures/client-log-upload-contract.json`.

- The host stores batches through `client_logs::ClientLogStore` (per device and stream cursors in `clients\<device-id>\cursor.json`) and reports through `feedback_store::FeedbackStore`, with a per-connection budget of 8 MiB.
- The browser (`dk-upload.js`), the Kotlin core (`LogUploader`, stream `app-android-core`) and the Dart app (`UploadCoordinator`, streams `app-win` and `app-android`) upload history first and then live lines. They send only when the control channel is idle: at most 16 KiB queued, no input in the last 250 ms, and one batch per 500 ms.
- Dart reaches the control channel through `CtlBridge`: flutter_rust_bridge on Windows (`ctl_log_send`, `ctl_log_state`, `ctl_log_events`) and pigeon on Android (`CtlHostApi`, `CtlFlutterApi`).
- When a report is acknowledged, the app shows "Sent to your PC."

## Status

Built: everything above. Reports and logs reach the PC only while a session is connected; nothing is sent otherwise.
Known gaps: the Android and Windows end-to-end tests run in CI only for the Kotlin side (they need the relay binary); the shake thresholds and the tray text box have not been seen on a real device or screen; the rest of `app/rust` outside `backend.rs` still logs nothing.

## Recovery line (2026-11-04)

After a lost signaling connection, each client logs one info line when the first frame is shown again: `recovered after <ms> ms, <n> attempts`, with the fields `recoveryMs` (loss to first frame) and `attempts` (signaling connections opened since the loss). Browser: target `receiver`; Android and the Windows client: the session log. It is not logged for a first connection. The signaling close lines carry `closeCode`. See [reconnect order](architecture/reconnect.md).

## Browser reconnect lines (2026-11-06)

While recovering, the browser logs one info line (target `receiver`) when it abandons a signaling socket that did not open in time, `signaling-connect-abandoned` with `connectAttempt` and `waitedMs`, and one when a wake event runs a pending reconnect, `reconnect-wake` with `event` (`visibilitychange`, `pageshow` or `online`). See [reconnect order](architecture/reconnect.md).

## Navigation type in the browser start line (2026-11-05)

The browser's `bootstrap-inline` trace (target `trace`, written once when the page starts) carries `navigation`: the page's `performance.getEntriesByType("navigation")[0].type`, one of `navigate`, `reload`, `back_forward` or `prerender`, or `null` when the browser gives none. A tab that restarts during recovery with `reload` was reloaded (by the user or the browser); `navigate` is a fresh load of the address. The value passes the redactor unchanged.

## Android background lines (2026-11-09)

The phone app's Kotlin core (`app-android-core`, target `Session`) logs one info line when the app goes to the background and media stops (`backgrounded: media stopped`, field `graceMs`), one when it returns within the 5 minute grace and the first live frame is shown (`resumed within grace: first frame after N ms`, fields `firstFrameMs`, `backgroundMs`) and one when the grace runs out and the session is disconnected (`grace expired: disconnected`, fields `backgroundMs`, `graceMs`). The field names avoid the redacted words; `RedactTest` checks they pass. See [reconnect](architecture/reconnect.md).

## Frozen foreground app (2026-11-09)

While a viewer is attached, the host checks the foreground window on the 250 ms foreground tick (`host/src/input_stall.rs`). When the window is hung (`IsHungAppWindow`), DWM's `Ghost` stand-in is foreground, or the cursor has stayed `Wait` for 5 s, it logs one warn line per episode: `foreground app stopped responding while a viewer is attached`. A new episode starts only after a sample with none of the three. Fields:

- `reason` (`hung`, `ghost` or `wait-cursor`), `foreground_exe`, `foreground_class`, `foreground_pid`, `wait_ms`;
- `behind_ghost_exe`, `behind_ghost_class`, `behind_ghost_pid`: the last non-ghost foreground, which is the app the ghost stands in for;
- `native_queue_attached`: native file drop threads whose input queue is attached to a foreground thread (`AttachThreadInput`) right now;
- `cursor_hook_busy_ms`, `cursor_hook_last_event_ms`: how long the cursor-wake thread (owner of the host's `WH_MOUSE_LL` hook while a viewer is attached) has been out of its message wait, 0 while it waits, and the time since it last woke; `drop_hook_busy_ms` is the same for the prepared-drop input monitor's hooks;
- `last_file_drop_step`, `last_clipboard_step`: the last step name, `running` or `done`, and its age. No clipboard or file content is logged.

The line was added after a WhatsApp freeze on 2026-11-09 that a TeamViewer connection ended; the host's logs then showed no attached queue or drop hook, so these fields say on the next occurrence whether the host was involved.
