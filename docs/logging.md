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

Consequence for authors: do not name log fields `code`, `key`, `auth` and the like (use `status`, `hresult`, `reason`); the value is masked.

## Where files live

On the PC, under `%LOCALAPPDATA%\BubblePlay\logs` (host, tray) or the protected install `logs` folder (admin agent, secure helper). Override with `BUBBLEPLAY_LOG_DIR`.

| Files | Contents | Retention |
|---|---|---|
| `host-YYYY-MM-DD.jsonl`, `tray-...`, `agent-...`, `helper-...` | the PC's own processes (`.N.jsonl` after 20 MiB) | 7 days, 20 files per source |
| `clients\<device-id>\YYYY-MM-DD.jsonl`, `device.json`, `cursor.json` | logs sent by paired clients | 14 days, 20 MiB per device, 200 MiB total |
| `feedback\<YYYYMMDDTHHMMSSmmmZ>-<device-id>.json` (+ `.png`) | feedback reports | kept |

The tray log also holds the host's and agent's raw stdout and stderr as lines with target `child`.

On clients: `logs/pc/<pc-id>/<src>-YYYY-MM-DD.jsonl` (lines tied to a PC, 5 MB per PC), `logs/misc/` (the rest), `logs/outbox/` (reports waiting to be sent). 20 MB total, 30 days. Roots: Windows app `%LOCALAPPDATA%\BubblePlay\viewer\logs`; Android the app's `files/logs`, shared by the Dart process (`app-android`) and the Kotlin core (`app-android-core`), each writing its own files. The browser keeps a 2 MB ring in IndexedDB `bp-logs` (`lines`, `feedback`).

The relay logs structured JSON to the console (Workers observability); `pc` and peer IDs are cut to 8 characters, and every refusal has a class (for example `pairing.wrong-code`) and a 1008 close reason clients can show.

## Feedback

"Report a problem" is in the Flutter app (Ctrl+Shift+F, Settings, session menu), the browser viewer (Ctrl+Shift+F, More, Settings) and the tray menu (opens the app's sheet over the devices pipe, `open-feedback` / `openFeedback`). A report holds the text, kind (bug, complaint, idea, other), timestamp, app version and commit, platform, device, PC, session, screen, the last 200 log lines, the last 20 errors and, in the app, a screenshot of the Flutter UI (never the stream). It is saved in the client outbox and sent to the PC the client is connected to.

## Finding what happened around a feedback timestamp

```
bubbleplay-logs feedback
bubbleplay-logs around 2026-11-02T13:45:12Z --window 5m [--src host,tray] [--device <id>]
```

`feedback` lists reports with time and first line. `around` merges the PC's own logs and all client logs in the window, sorted by `ts`, each line prefixed by its file.

## Status

Built: all of the above except the transfer of client logs and reports to the PC.
Not built yet: the session-channel upload (historical lines, live stream, cursor acks, feedback upload). The host stores batches through `client_logs::ClientLogStore` and `feedback_store::FeedbackStore`, which the session code will call. The Rust side of the Windows app (`app/rust/src/backend.rs`) still swallows three errors without logging.
