# Client log and feedback upload (wire protocol v1)

Clients send their logs and "Report a problem" reports to the PC they are connected to, over the authenticated end-to-end session control channel (the sealed `ctl` envelopes on `/ws`; see [device-keys.md](device-keys.md)). The relay forwards `ctl` kinds generically and never sees the contents. File layout, JSONL schema and redaction: [../logging.md](../logging.md).

## Negotiation

- Capability string `client-logs-v1`, advertised by both sides in the ctl hello like the existing caps (`control-aead-v1`, `dtls-bind-v1`). Missing on either side means off: nothing is sent and no message below is accepted.
- Every message carries `"v": 1`. A receiver ignores messages with an unknown `v` and unknown extra fields. A new incompatible version gets a new capability string (`client-logs-v2`).
- Messages are valid only after authentication, and only from a paired device. The host takes the device id from the authenticated session; any device id a client puts in a message or line is ignored (the host overwrites the `device` field of every stored line).

## Streams and cursors

A client process uploads one or more streams. A stream is the `src` of the lines it owns: `app-win`, `app-android`, `app-android-core`, `browser`. Each has its own cursor, so the Dart and Kotlin processes of the Android app upload independently.

A cursor is an opaque string of 1-64 characters from `[0-9A-Za-z#:._-]`, compared byte-wise. Within a stream, cursors must increase with every line written, and a batch's `to` is the cursor of its last line. Clients use fixed-width forms (for example `2026-11-02T13:45:12.345Z#000042`, or a 20-digit zero-padded counter). The host stores the last accepted cursor per (device, stream) in `clients/<device-id>/cursor.json`.

## Messages (JSON, inner `type`)

Host to client, once, right after authentication when the capability is on:

```json
{"type":"logs-state","v":1,"cursors":{"app-android-core":"<last accepted>"},"limits":{"maxBatchBytes":16384,"maxBatchLines":100,"connectionBudgetBytes":8388608,"minBatchIntervalMs":500}}
```

A stream missing from `cursors` has never uploaded: send everything held locally for the PCs it relates to (lines whose `pc` is this PC) plus lines with no `pc` newer than the cursor.

Client to host:

```json
{"type":"logs-batch","v":1,"stream":"app-android-core","id":17,"from":"<cursor before first line>","to":"<cursor of last line>","live":false,"lines":["{...jsonl...}","..."]}
```

- `lines` are redacted JSONL lines as written locally. `id` is a client counter per connection. `live` is true for lines streamed as they are written, false for history.
- One batch in flight per stream. The next batch is sent only after its ack.

Host to client:

```json
{"type":"logs-ack","v":1,"stream":"app-android-core","id":17,"status":"ok","cursor":"<stored cursor>","dropped":0,"retryAfterMs":0}
```

- `status`: `ok` (stored or already stored; resume from `cursor`), `rate-limited` (retry after `retryAfterMs`, same batch), `budget` (connection budget used up; stop until the next connection), `rejected` (batch invalid, do not retry it; `reason` is a short plain string). A batch whose `to` is not above the stored cursor is acknowledged `ok` without writing (resume after a dropped connection).
- `dropped` counts lines the host refused (invalid JSON, over 8 KiB, timestamp out of range).

Feedback reports (the report object is spec 4 of the charter, without the screenshot):

```json
{"type":"feedback-begin","v":1,"id":"<report uuid>","report":{...},"pngBytes":123456,"chunks":3}
{"type":"feedback-chunk","v":1,"id":"<report uuid>","i":0,"data":"<base64, at most 32768 bytes decoded>"}
```
Host to client: `{"type":"feedback-ack","v":1,"id":"<report uuid>","status":"ok|rejected|retry","reason":"..."}`. `pngBytes` 0 and `chunks` 0 means no screenshot. The host assembles chunks in memory (limit 5 MiB, one report at a time per device, 60 s between chunks), then stores the report through `FeedbackStore`. `ok` means it is on disk: the client then deletes it from its outbox. Duplicate report ids are acknowledged `ok` without storing again.

## Priority

Logging must never hurt the stream:

- A client sends a log or feedback message only when its ctl send buffer is at or below 16 KiB and no input was sent in the last 250 ms, and at most one such message per `minBatchIntervalMs`.
- Batches are at most 16 KiB and 100 lines. History goes first, oldest PC-related lines first; live lines are coalesced into one batch per interval.
- The host counts bytes per connection against `connectionBudgetBytes` (8 MiB) and answers `budget` when it is used up. It also applies the per-device caps of `ClientLogStore`.
- Either side may stop at any time; nothing in the protocol is required for a session to work.

## Adding or changing a message

Host: `SignalMsg` variants and the handler in `host/src/signaling.rs` and `dk_host.rs`. Browser: `dk-session.js`, `receiver.js`. Android: `CtlChannel.kt`, `Session.kt`. Windows client: `client-win/src/net`. Vectors for each message are in `fixtures/client-log-upload-contract.json`; all implementations test against them.
