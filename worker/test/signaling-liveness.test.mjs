import test from "node:test";
import assert from "node:assert/strict";
import { MEDIA_ANSWER_LIMIT_MS, SIGNALING_ANSWER_LIMIT_MS, mediaHeartbeatStep, signalingUnanswered } from "../public/recovery.js";

// Drives receiver.js's heartbeat rule on a fake clock: each tick checks, then pings
// (keeping the oldest unanswered ping), exactly as the interval in receiver.js does.
function heartbeat() {
  const state = { pingSentMs: null, inboundMs: 0, recycled: [], pings: 0 };
  return {
    state,
    tick(nowMs) {
      if (signalingUnanswered({ pingSentMs: state.pingSentMs, inboundMs: state.inboundMs, nowMs })) {
        state.pingSentMs = null;
        state.recycled.push(nowMs);
        return;
      }
      if (state.pingSentMs === null || state.inboundMs >= state.pingSentMs) state.pingSentMs = nowMs;
      state.pings++;
    },
    answer(nowMs) { state.inboundMs = nowMs; },
  };
}

test("an answered heartbeat never recycles, even for hours or with hidden-tab throttling", () => {
  for (const every of [15_000, 60_000]) {
    const h = heartbeat();
    for (let now = 1; now < 3 * 3_600_000; now += every) {
      h.tick(now);
      h.answer(now + 80);
    }
    assert.deepEqual(h.state.recycled, [], `every ${every} ms`);
  }
});

test("a half-open socket (pings never answered) recycles within two heartbeats past the limit", () => {
  const h = heartbeat();
  let now = 1;
  for (; now < 600_000; now += 15_000) {
    h.tick(now);
    h.answer(now + 80);
  }
  const deadAt = now;
  for (; now < deadAt + 120_000 && h.state.recycled.length === 0; now += 15_000) h.tick(now);
  assert.equal(h.state.recycled.length, 1);
  const after = h.state.recycled[0] - deadAt;
  assert.ok(after >= SIGNALING_ANSWER_LIMIT_MS && after <= SIGNALING_ANSWER_LIMIT_MS + 15_000, `recycled after ${after} ms`);
});

test("signalingUnanswered needs an outstanding ping older than the limit", () => {
  assert.equal(signalingUnanswered({ pingSentMs: null, inboundMs: 0, nowMs: 1e9 }), false);
  assert.equal(signalingUnanswered({ pingSentMs: 100, inboundMs: 100, nowMs: 1e9 }), false);
  assert.equal(signalingUnanswered({ pingSentMs: 100, inboundMs: 99, nowMs: 100 + SIGNALING_ANSWER_LIMIT_MS - 1 }), false);
  assert.equal(signalingUnanswered({ pingSentMs: 100, inboundMs: 99, nowMs: 100 + SIGNALING_ANSWER_LIMIT_MS }), true);
});

// The WSS v4 media socket: a static desktop sends the viewer only a frame or so a minute while the
// viewer acks every few seconds (acks are not modelled: only inbound traffic counts). Pings must not wait for the viewer to be quiet, or the socket is
// closed as "silent" although the relay would answer. Drives the receiver's 1 s tick on a fake clock.
function mediaSocket({ tickMs, relayAnswers }) {
  const state = { pingSentMs: null, lastPingAt: 0, lastInboundAt: 0 };
  let closedAt = null;
  const hostFrameEveryMs = 60_000;
  for (let now = 1_000; now < 30 * 60_000 && closedAt === null; now += tickMs) {
    if (now % hostFrameEveryMs < tickMs) state.lastInboundAt = now;
    const step = mediaHeartbeatStep(state, now, { heartbeatMs: 15_000 });
    if (step === "dead") closedAt = now;
    else if (step === "ping" && relayAnswers(now)) state.lastInboundAt = now + 80;
  }
  return closedAt;
}

test("a media socket the relay keeps answering is never closed, on a static desktop or in a throttled tab", () => {
  for (const tickMs of [1_000, 60_000]) {
    assert.equal(mediaSocket({ tickMs, relayAnswers: () => true }), null, `tick ${tickMs} ms`);
  }
});

test("a media socket whose pings go unanswered is closed within the limit, as before", () => {
  const deadFrom = 10 * 60_000;
  const closedAt = mediaSocket({ tickMs: 1_000, relayAnswers: now => now < deadFrom });
  const after = closedAt - deadFrom;
  assert.ok(after > 0 && after <= 15_000 + MEDIA_ANSWER_LIMIT_MS + 1_000, `closed ${after} ms after the last pong`);
});
