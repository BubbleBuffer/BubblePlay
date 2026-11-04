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

When the first frame is shown again each client logs one info line with the total recovery time and the attempt count ([logging](../logging.md)). The signaling close code is logged as `closeCode`.

## Not covered

The media session does not survive a signaling failure: a signaling reset still rebuilds the media link. Keeping it alive needs a protocol change and is a separate decision.
