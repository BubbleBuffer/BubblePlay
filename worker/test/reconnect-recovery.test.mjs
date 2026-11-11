import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createViewerSession } from "../public/dk-session.js";
import {
  RECOVERY_BUDGET_MS, recycledAuthTimeoutMs, shouldRetryAuthTimeout, connectTimeoutMs, shouldRetryOnWake,
  shouldReconnectAfterClose, createSuspendTracker, splitRecovery,
} from "../public/recovery.js";

const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").split(String.fromCharCode(13, 10)).join("\n");
const fn = name => {
  const start = source.indexOf(`function ${name}(`);
  assert.ok(start >= 0, name);
  return source.slice(start, source.indexOf("\n}\n", start) + 3);
};

test("a recycled session waits 3 s, backing off to a cap of 8 s", () => {
  assert.deepEqual([0, 1, 2, 3, 9].map(recycledAuthTimeoutMs), [3000, 4500, 6750, 8000, 8000]);
  assert.equal(recycledAuthTimeoutMs(-1), 3000);
});

test("only a silent PC of an established session is retried, and only inside the budget", () => {
  const base = { reason: "auth-timeout", established: true, budgetStartMs: 1000, nowMs: 2000 };
  assert.equal(shouldRetryAuthTimeout(base), true);
  assert.equal(shouldRetryAuthTimeout({ ...base, established: false }), false, "first connection keeps today's terminal timeout");
  assert.equal(shouldRetryAuthTimeout({ ...base, nowMs: 1000 + RECOVERY_BUDGET_MS }), false);
  assert.equal(shouldRetryAuthTimeout({ ...base, budgetStartMs: null }), false);
  for (const reason of ["pc-key-changed", "handshake-failed", "bind-failed", "missing-cap:control-aead-v1", "dtls-bind-required", "cap-missing", "control-failed"]) {
    assert.equal(shouldRetryAuthTimeout({ ...base, reason }), false, reason);
  }
});

test("the session honours the shorter auth-host wait", async () => {
  const timers = [];
  const session = createViewerSession({
    identity: { id: "x", publicRaw: new Uint8Array(32), privateKey: null }, pcId: "pc",
    pinnedPublic: new Uint8Array(32), relayOrigin: "https://relay.test", epoch: "epoch", send: () => true,
    schedule: (fn, delay) => { timers.push(delay); return timers.length; }, cancel() {}, authTimeoutMs: 3000,
  });
  await session.start();
  assert.equal(timers[0], 3000);
});

function receiverHarness() {
  const output = { ended: [], recycled: [], logs: [] };
  let now = 100_000;
  const context = vm.createContext({
    everAuthenticated: false, recoveryLossMs: null, recoveryBudgetStartMs: null, recoveryAttempts: 0,
    performance: { now: () => now }, Math, suspendTracker: createSuspendTracker(), splitRecovery, recoverySuspendedAtLossMs: 0, recycledAuthTimeoutMs, shouldRetryAuthTimeout,
    viewerLog: (level, event, detail) => output.logs.push({ level, event, detail }),
    recycleSignaling: reason => output.recycled.push(reason),
    endAuthenticatedSession: refusal => output.ended.push(refusal),
  });
  vm.runInContext(["noteConnectionLoss", "noteSessionAuthenticated", "sessionAuthTimeoutMs", "noteRecovered", "handleSessionFailure"].map(fn).join("\n"), context);
  return { context, output, advance: ms => { now += ms; } };
}

test("auth timeout on a first connection ends the session as before", () => {
  const h = receiverHarness();
  assert.equal(h.context.sessionAuthTimeoutMs(), undefined);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.equal(h.output.ended.length, 1);
  assert.deepEqual(h.output.recycled, []);
});

test("auth timeout after a loss retries with a shorter, growing wait until the budget is spent", () => {
  const h = receiverHarness();
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss(); // peer-left
  h.context.recoveryAttempts = 1; // first reconnect socket opened
  assert.equal(h.context.sessionAuthTimeoutMs(), 3000);
  h.advance(3500);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.deepEqual(h.output.recycled, ["auth-timeout"]);
  assert.equal(h.output.ended.length, 0);
  h.context.recoveryAttempts = 2;
  assert.equal(h.context.sessionAuthTimeoutMs(), 4500);
  h.advance(RECOVERY_BUDGET_MS);
  h.context.handleSessionFailure({ title: "PC not reachable" }, "auth-timeout");
  assert.equal(h.output.ended.length, 1, "the budget is spent: the existing terminal screen");
  assert.equal(h.output.recycled.length, 1);
});

test("a real refusal after a loss is never retried", () => {
  const h = receiverHarness();
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss();
  h.context.handleSessionFailure({ title: "PC key changed" }, "pc-key-changed");
  assert.equal(h.output.ended.length, 1);
  assert.deepEqual(h.output.recycled, []);
});

test("one info line reports the total time and attempts when the first frame returns", () => {
  const h = receiverHarness();
  h.context.noteRecovered();
  assert.deepEqual(h.output.logs, [], "nothing on a first connection");
  h.context.noteSessionAuthenticated();
  h.context.noteConnectionLoss();
  h.context.recoveryAttempts = 2;
  h.advance(3120);
  h.context.noteRecovered();
  h.context.noteRecovered();
  assert.equal(h.output.logs.length, 1);
  assert.equal(h.output.logs[0].level, "info");
  assert.deepEqual({ ...h.output.logs[0].detail }, { recoveryMs: 3120, activeMs: 3120, suspendedMs: 0, attempts: 2 });
  assert.equal(h.context.recoveryLossMs, null);
});

test("the recovery line reports frozen or suspended time apart from active time", () => {
  const h = receiverHarness();
  h.context.noteSessionAuthenticated();
  h.context.suspendTracker.tick(100_000);
  h.context.noteConnectionLoss();
  h.advance(856_700); // the PC sleeps; no tick fires meanwhile
  h.context.suspendTracker.tick(100_000 + 856_700);
  h.advance(3_900);
  h.context.noteRecovered();
  assert.deepEqual({ ...h.output.logs[0].detail }, { recoveryMs: 860_600, activeMs: 3_900, suspendedMs: 856_700, attempts: 0 });
  assert.match(h.output.logs[0].event, /^recovered after 3900 ms/);
});

test("the connect wait is 5 s, 7.5 s, then 10 s, and only while recovering", () => {
  assert.deepEqual([0, 1, 2, 3, 9].map(attempt => connectTimeoutMs({ recovering: true, attempt })), [5000, 7500, 10000, 10000, 10000]);
  assert.equal(connectTimeoutMs({ recovering: false, attempt: 0 }), null);
});

test("wake retries only a pending reconnect with no socket connecting", () => {
  assert.equal(shouldRetryOnWake({ timerPending: true, terminal: false, connecting: false }), true);
  assert.equal(shouldRetryOnWake({ timerPending: true, terminal: false, connecting: true }), false);
  assert.equal(shouldRetryOnWake({ timerPending: false, terminal: false, connecting: false }), false);
  assert.equal(shouldRetryOnWake({ timerPending: true, terminal: true, connecting: false }), false);
});

// ---- reconnectSignaling with a fake WebSocket and a virtual clock ---------------------------

function signalingHarness({ recovering }) {
  const sockets = [];
  const logs = [];
  const sent = [];
  let clock = 0, nextId = 1;
  let timers = [];
  class FakeSocket {
    static CONNECTING = 0; static OPEN = 1; static CLOSING = 2; static CLOSED = 3;
    constructor(url) { this.url = url; this.readyState = 0; this.closed = false; sockets.push(this); }
    close() { this.closed = true; this.readyState = 3; }
  }
  const context = vm.createContext({
    ws: null, signalingGeneration: 0, reconnectTimer: null, pendingConnect: null, connectTimer: null,
    terminalSignalingFailure: false, recoveryLossMs: recovering ? 1 : null, recoveryAttempts: 0,
    hudState: {}, latestInputStatus: null, inputStatusReason: null, authenticatedSession: null,
    signalingRegistered: false, signalingPc: null, signalingTicket: null, sessionIdentity: {}, params: {},
    hud: { classList: { add() {} } },
    WebSocket: FakeSocket, URL, Promise, Math, Number, Array,
    connectTimeoutMs, shouldRetryOnWake, shouldReconnectAfterClose,
    setTimeout: (fn, delay) => { const id = nextId++; timers.push({ id, at: clock + delay, fn }); return id; },
    clearTimeout: id => { timers = timers.filter(t => t.id !== id); },
    signalingSocketUrl: () => "wss://relay.test/ws?pc=pc",
    debugTrace() {}, viewerLog: (level, event, detail) => logs.push({ level, event, detail: { ...detail } }),
    sendRelay: message => sent.push(message), viewerHello: () => ({ type: "hello" }), wantsTurn: () => false,
    handleSignal: async () => {}, noteConnectionLoss() {}, cleanupPeer() {}, recordSignalingError() {},
    releaseInput() {}, showNotAdmitted() {}, restartPeer() {}, performance: { now: () => clock },
  });
  vm.runInContext(["reconnectSignaling", "armConnectTimeout", "wakeReconnect"].map(fn).join("\n"), context);
  const tick = ms => {
    const end = clock + ms;
    for (;;) {
      const due = timers.filter(t => t.at <= end).sort((a, b) => a.at - b.at)[0];
      if (!due) break;
      timers = timers.filter(t => t !== due);
      clock = due.at;
      due.fn();
    }
    clock = end;
  };
  return { context, sockets, logs, sent, tick };
}

test("a socket that does not open is abandoned and replaced, waiting longer each time", () => {
  const h = signalingHarness({ recovering: true });
  h.context.reconnectSignaling("ws-closed");
  h.tick(250);
  assert.equal(h.sockets.length, 1);
  const first = h.sockets[0];
  const lateOpen = first.onopen;
  h.tick(4999);
  assert.equal(h.sockets.length, 1, "still waiting at 4.999 s");
  h.tick(1);
  assert.equal(first.closed, true);
  assert.equal(first.onopen, null);
  assert.equal(first.onclose, null);
  assert.deepEqual(h.logs.map(l => [l.event, l.detail]), [["signaling-connect-abandoned", { connectAttempt: 1, waitedMs: 5000 }]]);
  h.tick(250);
  assert.equal(h.sockets.length, 2);
  assert.equal(h.context.ws, h.sockets[1]);
  // A late open event of the abandoned socket does nothing.
  lateOpen();
  assert.deepEqual(h.sent, []);
  assert.equal(h.context.ws, h.sockets[1]);
  h.tick(7500);
  h.tick(250);
  assert.equal(h.sockets.length, 3);
  h.tick(10_000);
  h.tick(250);
  h.tick(10_000);
  assert.deepEqual(h.logs.map(l => l.detail.waitedMs), [5000, 7500, 10000, 10000]);
});

test("a socket that opens in time is kept", () => {
  const h = signalingHarness({ recovering: true });
  h.context.reconnectSignaling("ws-closed");
  h.tick(250);
  h.tick(2000);
  h.sockets[0].readyState = 1;
  h.sockets[0].onopen();
  h.tick(60_000);
  assert.equal(h.sockets.length, 1);
  assert.equal(h.sockets[0].closed, false);
  assert.deepEqual(h.logs, []);
});

test("a first connection is not timed", () => {
  const h = signalingHarness({ recovering: false });
  h.context.reconnectSignaling("startup");
  assert.equal(h.sockets.length, 1);
  h.tick(120_000);
  assert.equal(h.sockets.length, 1);
  assert.equal(h.sockets[0].closed, false);
  assert.deepEqual(h.logs, []);
});

test("waking retries a pending reconnect at once and logs the event", () => {
  const h = signalingHarness({ recovering: true });
  h.context.reconnectSignaling("ws-closed");
  assert.equal(h.sockets.length, 0);
  h.context.wakeReconnect("visibilitychange");
  assert.equal(h.sockets.length, 1);
  assert.deepEqual(h.logs.map(l => [l.event, l.detail]), [["reconnect-wake", { event: "visibilitychange" }]]);
  h.tick(250);
  assert.equal(h.sockets.length, 1, "the cleared retry timer does not open a second socket");
});

test("waking does nothing while a socket is connecting, when nothing is pending, or after the end", () => {
  const h = signalingHarness({ recovering: true });
  h.context.wakeReconnect("online");
  assert.equal(h.sockets.length, 0, "nothing pending");
  h.context.reconnectSignaling("ws-closed");
  h.tick(250);
  h.context.wakeReconnect("pageshow");
  assert.equal(h.sockets.length, 1, "already connecting");
  assert.deepEqual(h.logs, []);
  const ended = signalingHarness({ recovering: true });
  ended.context.reconnectSignaling("ws-closed");
  ended.context.terminalSignalingFailure = true;
  ended.context.wakeReconnect("online");
  assert.equal(ended.sockets.length, 0);
});

test("a policy refusal is terminal: no retry, no wake, no abandon timer", async () => {
  const h = signalingHarness({ recovering: true });
  h.context.reconnectSignaling("ws-closed");
  h.tick(250);
  h.sockets[0].readyState = 3;
  h.sockets[0].onclose({ code: 1008, reason: "refused" });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.context.terminalSignalingFailure, true);
  h.context.wakeReconnect("online");
  h.tick(60_000);
  assert.equal(h.sockets.length, 1);
  assert.deepEqual(h.logs.filter(l => l.event !== "signaling-close"), []);
});
