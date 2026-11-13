import test from "node:test";
import assert from "node:assert/strict";
import { createSessionState } from "../public/session-state.js";
import { createSignalingSocket } from "../public/signaling-socket.js";
import { createRecoveryTracker } from "../public/recovery-tracker.js";

import { CLOSE_POLICY } from "../public/recovery.js";
const CLOSE_ABNORMAL = 1006;
const FIRST_BACKOFF_MS = 250;

function harness(overrides = {}) {
  const state = createSessionState({ wssSelected: false });
  const sockets = [], calls = [], timers = [];
  class FakeSocket {
    constructor(url) { this.url = url; this.readyState = 0; this.sent = []; sockets.push(this); }
    send(text) { this.sent.push(JSON.parse(text)); }
    close(code, reason) { this.readyState = 3; this.closedWith = { code, reason }; }
  }
  const signaling = createSignalingSocket({
    state, recovery: createRecoveryTracker({ now: () => 0, log: () => {} }), hudState: {},
    url: () => "wss://relay.test/ws?pc=pc",
    hello: () => ({ type: "hello" }),
    handleSignal: async () => {},
    cleanupPeer: () => calls.push(["cleanup-peer"]),
    restartPeer: (reason, reconnect) => calls.push(["restart-peer", reason, reconnect]),
    markBad: () => calls.push(["bad"]),
    onPolicyClose: details => calls.push(["policy-close", details]),
    onMessageFailed: error => calls.push(["message-failed", error.message]),
    invalidateMonitor: reason => calls.push(["invalidate-monitor", reason]),
    recycleDiagnostic: reason => {
      calls.push(["diagnostic", state.ws === sockets.at(-1), state.signalingGeneration]);
      return { reason };
    },
    log: () => {}, trace: (stage, detail) => calls.push(["trace", stage, detail]), recordError: () => {},
    now: () => 0, makeSocket: url => new FakeSocket(url), random: () => 0.5,
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; }, clearTimeout: () => {},
    ...overrides,
  });
  const flush = async () => { for (let i = 0; i < 4; i++) await Promise.resolve(); };
  return { state, signaling, sockets, calls, timers, flush };
}

test("a first connection opens at once and sends the hello when the socket opens", () => {
  const h = harness();
  h.signaling.reconnect("startup");
  assert.equal(h.sockets.length, 1);
  assert.equal(h.timers.length, 0);
  h.sockets[0].readyState = 1;
  h.sockets[0].onopen();
  assert.deepEqual(h.sockets[0].sent, [{ type: "hello" }]);
  assert.deepEqual(h.calls.filter(c => c[0] === "trace").map(c => c[1]), ["signaling-construct", "signaling-open"]);
});

test("a transient close drops the session, restarts the peer and reconnects after the backoff", async () => {
  const h = harness();
  h.signaling.reconnect("startup");
  const socket = h.sockets[0];
  h.state.signalingTicket = "ticket";
  h.state.signalingRegistered = true;
  socket.readyState = 3;
  socket.onclose({ code: CLOSE_ABNORMAL });
  await h.flush();
  assert.equal(h.state.signalingTicket, null);
  assert.equal(h.state.signalingRegistered, false);
  assert.deepEqual(h.calls.filter(c => c[0] === "cleanup-peer" || c[0] === "restart-peer"), [["cleanup-peer"], ["restart-peer", "ws-closed", false]]);
  assert.equal(h.sockets.length, 1);
  assert.equal(h.timers[0].delay, FIRST_BACKOFF_MS);
  h.timers[0].fn();
  assert.equal(h.sockets.length, 2);
});

test("a policy close is terminal and says whether the viewer had been admitted", async () => {
  for (const admitted of [false, true]) {
    const h = harness();
    h.signaling.reconnect("startup");
    h.state.signalingRegistered = admitted;
    h.sockets[0].readyState = 3;
    h.sockets[0].onclose({ code: CLOSE_POLICY, reason: "refused" });
    await h.flush();
    assert.equal(h.state.terminalSignalingFailure, true);
    assert.deepEqual(h.calls.filter(c => c[0] === "policy-close"), [["policy-close", { admitted, reason: "refused" }]]);
    assert.equal(h.timers.length, 0);
    h.signaling.reconnect("ws-closed");
    h.signaling.recycle("again");
    assert.equal(h.sockets.length, 1);
  }
});

test("a failed message handler is reported only while its socket is current", async () => {
  const h = harness({ handleSignal: async () => { throw new Error("integrity"); } });
  h.signaling.reconnect("startup");
  const socket = h.sockets[0];
  socket.onmessage({ data: "{}" });
  await h.flush();
  assert.deepEqual(h.calls.filter(c => c[0] === "message-failed"), [["message-failed", "integrity"]]);
  h.state.ws = null;
  socket.onmessage({ data: "{}" });
  await h.flush();
  assert.equal(h.calls.filter(c => c[0] === "message-failed").length, 1);
});

test("a recycle reads its diagnostic from the old socket, closes it with the reason and reconnects", () => {
  const h = harness();
  h.signaling.reconnect("startup");
  const old = h.sockets[0];
  old.readyState = 1;
  h.state.signalingRegistered = true;
  h.state.signalingTicket = "ticket";
  const generation = h.state.signalingGeneration;
  h.signaling.recycle("peer-left");
  assert.deepEqual(h.calls.filter(c => c[0] === "invalidate-monitor" || c[0] === "diagnostic"),
    [["invalidate-monitor", "peer-left"], ["diagnostic", true, generation]]);
  assert.equal(old.closedWith.reason, "peer-left");
  assert.equal(h.state.signalingTicket, null);
  assert.equal(h.state.signalingRegistered, false);
  assert.equal(h.state.signalingGeneration, generation + 1);
  assert.equal(h.state.lastSignalingRecycle.reason, "peer-left");
  assert.equal(typeof h.state.lastSignalingRecycle.atMs, "number");
  assert.equal(h.timers.length, 1, "the new connection waits for the backoff");
});
