# MVP: desktop browser over WSS — 2026-09-30

Goal: use the home desktop (ChatGPT / Claude apps) from a desktop Chromium
browser on a corporate network, where only HTTPS/WSS on 443 through the
Cloudflare Worker is reliable. WebRTC is out of scope for this MVP; it must keep
compiling and passing its existing tests, but is not changed or validated here.

Success = an all-day session that stays up, costs ~nothing while the screen is
static, looks sharp once the screen settles, and has correct mouse, keyboard
and clipboard behaviour.

Base: branch `mvp/wss-desktop` from `codex/stabilize-runtime` (5154b2a).

## Why not fix v3

v3 shards each frame over 16 TLS sockets / 16 Durable Objects with XOR parity
and expires late frames. On TCP, congestion is delay, not loss, so v3 turns a
slow link into dependency loss, forced IDRs and larger backlogs. v3 has no
bitrate adaptation (`wss_quality.sample` is v2-only). Replace, don't repair.
Leave v2/v3 code in place (removal is a later cleanup) but stop selecting them
for production browser sessions once v4 is negotiated.

## Media protocol v4 (one socket)

- One media WebSocket per side per session through ONE MediaRoom DO
  (existing `/host-media-ws` + `/media-ws` routes with `v=4`; the relay pairs
  host/client by epoch as today, stripe fixed at 0). Relay stays dumb, but it
  must not silently eat frames: if there is no peer, it closes/ignores and the
  host learns via the missing ACKs.
- Host→client binary message = one whole encoded access unit:
  small fixed header (magic, version=4, flags{key, refine}, generation u32,
  seq u32, capture_ts_us u64, width u16, height u16, codec u8) + Annex‑B bytes.
  No sharding, no FEC, no per-lane state.
- Client→host on the SAME socket: ACK `{seq, recv_ts, decoded}` for every frame
  (tiny, per frame, not batched on a 100 ms timer), keyframe request
  `{reason}`, and heartbeat. Put the format in `fixtures/` and test it from
  both Rust and JS, like `wss-contract.json`.
- Heartbeat: both media and signaling sockets send an app-level ping every
  15 s when otherwise idle (corporate proxies kill idle sockets at ~30–60 s).
  A static screen must not look dead.

## Host send policy (latency + stability)

- Latest-wins: never queue more than one un-sent encoded frame. Budget in-flight
  data by ACKs: if un-ACKed bytes > ~150 ms at current bitrate or un-ACKed
  frames > 4, SKIP capture/encode for this tick (don't queue, don't drop sent
  data). Frames already handed to the socket are always delivered by TCP.
- Rate control: delay-based. Track ACK RTT min over a sliding window; if RTT
  grows > 1.5× min + 40 ms or skips happen, step NVENC bitrate down (×0.8, floor
  ~1 Mbps); when clean for ~2 s, step up (×1.1, cap = profile max). Use
  `Nvenc::set_bitrate` (reconfigure, no IDR).
- Keyframes: first frame of a generation, a new peer / reconnect, or an
  explicit client request (decoder error / seq gap). No periodic or safety
  keyframe, no cached-IDR replay on v4. An ACK stall (3 s) only forgets the
  in-flight window. A v4 IDR is encoded at 4× the live bitrate (the one-frame
  VBV grows with it) so it is not blocky. (Decided 2026-09-30: the 30 s safety
  IDR caused the whole-screen "flicker" the owner reported.)
- Timer: WSS epochs take the same 1 ms timer period RAII as RTC epochs (the
  ~15 ms Windows wait otherwise caps fps and adds latency).

## Static-region optimization

DXGI duplication only produces frames on change; capture already exposes
damage rects and idle snapshots. For v4:

- Encode only when there is damage (or a keyframe/refine is due). A static
  screen sends nothing but heartbeats.
- Damage-aware encode: pass damage to the existing damage/QP-map path so
  unchanged macroblocks are skipped cheaply (verify what `FrameDamage` does
  today; use it if it's production-safe, otherwise rely on NVENC's own skip).
- Refinement (current, 2026-09-30, supersedes one whole-frame refine per
  settle): a 64×64 tile tracker (`host/src/tile_quality.rs`). Dirty tiles come
  from the DXGI dirty/move rects, accumulated until a frame is encoded; up to
  40 % of the frame they are read back (damage-sized copies into a reused
  staging texture) and hashed (`host/src/tile_hash.rs`). A repaint whose
  hashes all match what was last encoded is skipped. Tiles encoded while
  NVENC's frameAvgQP > 28 carry debt; debt tiles unchanged for 300 ms are
  refined together in one P frame through the HEVC delta-QP map (−6 on debt
  tiles, +2 elsewhere, 4× bitrate), at most every 500 ms and never during large
  motion. A whole-frame refine happens only when ≥ 50 % of tiles are in debt or
  the encoder has no QP map. Refines ride the idle follow-up snapshot.
- Cap: work mode 30 fps max; nothing sent when idle.

## Receiver

- One decode path: WebCodecs `VideoDecoder` (`optimizeForLatency: true`),
  HEVC if `isConfigSupported`, else H.264. Decode EVERY frame in order. Never
  expire frames on a deadline. If the decoder queue is deep, skip *presenting*
  (draw only the newest decoded frame), don't reset.
- Decoder error or gap in seq → request keyframe, discard deltas until key.
- Socket drop → reconnect with backoff (250 ms → 5 s), same signaling session
  where possible, request keyframe; the page must not require a reload.
- Visible status: connected / reconnecting / paused-static is fine (no
  "waiting for stream" when the screen is merely static).

## Input (desktop browser)

- Control events (keys, buttons, clipboard) are NEVER dropped. Today WSS input
  is dropped when `ws.bufferedAmount > 16 KiB` (`receiver.js` `sendInput`);
  that causes stuck/missing keys. Only mouse motion may be coalesced.
- Keyboard by physical scan code (`e.code` → set‑1 scan code with
  `KEYEVENTF_SCANCODE` + `KEYEVENTF_EXTENDEDKEY` where needed: arrows, Ins/Del,
  Home/End, PgUp/PgDn, right Ctrl/Alt, NumpadEnter/Divide, Win keys). Fixes
  AltGr and non‑US layouts and apps that ignore VK-only events. Distinguish
  left/right modifiers.
- Keyboard Lock (`navigator.keyboard.lock()`) while fullscreen so Ctrl+W/T/N,
  Alt+Tab-ish and the Win key reach the host where the browser allows it.
- Release all keys/buttons on blur, visibility loss and socket drop (already
  partly there); host also releases on session end.
- Clipboard, text only, both directions:
  - Local paste (Ctrl/Cmd+V or paste event over the stream) → send text →
    host sets the interactive-session clipboard → injects Ctrl+V.
  - Host clipboard change (in the interactive session) → send text (cap
    ~1 MiB) → browser `navigator.clipboard.writeText` when focused; if denied,
    show a "copy" affordance.
  - Clipboard must be set from the interactive user session (input proxy /
    agent), not session 0. Never on the secure desktop.
- Absolute mouse over the video's content rect (already there); wheel with
  high-resolution deltas accumulated to 120 units; local cursor stays visible
  (browser pointer), no pointer lock in desktop mode.

## Acceptance

Deterministic: `node tools/check.mjs full` green, new unit tests for v4 framing,
send policy, rate control, scan-code mapping and clipboard messages.

Live (local, isolated relay via existing stream-lab tooling or a manual
`wrangler dev` + candidate host run): v4 WSS session reaches first frame;
static screen → zero media bytes except heartbeats over 60 s; type into a
browser text box on the host and see it; paste round trip; forced socket drop
recovers without reload; 10‑minute soak with no decoder reset. Production
deployment and service install need the owner (elevation) and are separate.
