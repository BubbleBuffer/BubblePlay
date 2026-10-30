import test from "node:test";
import assert from "node:assert/strict";
import { SIGNALING_ANSWER_LIMIT_MS, signalingUnanswered } from "../public/recovery.js";

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
