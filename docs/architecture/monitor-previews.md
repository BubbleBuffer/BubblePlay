# Monitor previews (wire protocol v1)

Low-resolution snapshots of the monitors the host is not streaming. On a monitor switch the client shows the target monitor's latest snapshot at once, scaled up, and fades to live video on the first live frame. An unchanged monitor sends nothing. The snapshots travel over the authenticated end-to-end session control channel (the sealed `ctl` envelopes on `/ws`; see [device-keys.md](device-keys.md)), like [client-log-upload.md](client-log-upload.md); the relay forwards them generically and never sees pictures. Vectors: `fixtures/monitor-previews-contract.json`.

## Negotiation

- Capability string `monitor-previews-v1`, advertised by both sides in the ctl `caps` (the host always, the clients that can show them). Missing on the viewer side: the host sends nothing. A client that does not know the message ignores it like any unknown ctl type.
- Every message carries `"v": 1`. A receiver ignores unknown `v` and unknown extra fields. An incompatible change gets a new capability string.

## Message (host to client)

```json
{"type":"monitor-preview","v":1,"monitor":"\\\\.\\DISPLAY2","seq":7,"w":640,"h":360,"jpeg":"<base64>"}
```

- `monitor` is the `selector` of an entry of the `monitor-list` (`\\.\DISPLAYn`). `seq` rises by one per message in a session. `w` x `h` is the picture size: height at most 360, width at most 960, in the monitor's aspect ratio. `jpeg` is a baseline JPEG, at most 96 KiB decoded (about 20-40 KB for a desktop at quality 60).
- No acknowledgement. The newest picture of a monitor wins; one lost on the way is replaced by the next significant change. Clients hold one picture per monitor selector, in memory only, and drop everything when the session ends. A message with an older `seq` than the held picture for that monitor, bad base64, a non-JPEG body, a zero size or a missing field is ignored.

## Host behaviour

`host/src/monitor_previews.rs` (decisions, JPEG, message), `host/src/monitor_preview_capture.rs` (taking the pictures) and the sender wiring in `host/src/main.rs`.

- **Which monitors.** Every monitor in the list except the one being streamed, while a viewer with the capability is attached. A monitor that becomes non-streamed gets a fresh first snapshot; the streamed one gets none.
- **Taking the picture.** A GDI stretch of the monitor's display DC into a 360p DIB (half-tone), on a worker thread, at most once per second per monitor. It uses no second Desktop Duplication session and no encoder, and nothing waits for it. Nothing is taken while the input desktop is not the default one (lock screen, UAC).
- **Significant change.** The picture is compared with the last one sent on a grid of 16 px cells: a cell counts as changed when its mean luma moved by 10 or more. A new picture is sent when at least 2 % of the cells (and at least 6) changed. A blinking cursor, a ticking clock or a tray icon touches fewer cells. The first picture of a monitor is always sent. All thresholds are constants in `monitor_previews.rs` with tests. (DXGI dirty rectangles of the other monitors are not used: that would need a second duplication session per monitor, which the lane rules out.)
- **Only while the link is stable.** Nothing is sent while a switch is waiting for its first frame, while the WSS link reports a constrained capacity, within 10 s of a reconnect or of a quality drop, or while no viewer media is live. The decision is made for every picture; gated pictures are not queued.
- **Rate.** At most one snapshot per monitor per second and a byte budget of 48 KiB/s (burst 160 KiB) per session. The send is the last thing the main loop does each tick, so it never delays video or input.
- **Log.** One info line when the viewer session ends: `monitor previews: sent, bytes, skipped_unchanged, skipped_gated, skipped_budget, capture_failures`.

## Client behaviour

See `clientBehaviour` in the fixture. In short: keep the latest picture per monitor; when the selected monitor changes (by the user's pick, by follow, or by an edge drag) and a picture of the new one is held, show it scaled up at once and fade to live on the first live frame; drop the held picture of the monitor that is live. No snapshot held: behave as before. The input gate after a switch is unchanged. No settings.

## Adding or changing a message

Host: the `monitor-previews-v1` cap in `host/src/dk_host.rs`, the sender in `host/src/main.rs`. Browser: `worker/public/dk-session.js`, `receiver.js`. Android: `net/Session.kt`. Windows app: `client-win/src/net`, `app/rust`, `app/lib/src/screens`.
