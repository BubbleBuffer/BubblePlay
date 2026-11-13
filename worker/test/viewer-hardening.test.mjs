import test from "node:test";
import { attachSignaling } from "./helpers/signaling-wiring.mjs";
import { createSessionContext } from "./helpers/session-context.mjs";
import { recoveryStub } from "./helpers/recovery-stub.mjs";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { V4_BACKOFF_JITTER_FRACTION, V4_BACKOFF_MAX_MS, V4_BACKOFF_MIN_MS } from "../public/media-v4.js";

// Behaviour of receiver.js paths that are not modules yet, run in a vm sandbox as the other receiver tests do.
const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");

function receiverFunctions(...names) {
  return names.map(name => {
    const found = source.indexOf(`function ${name}(`);
    assert.ok(found >= 0, `receiver function ${name}`);
    const start = source.startsWith("async ", found - 6) ? found - 6 : found;
    return source.slice(start, source.indexOf("\n}\n", start) + 3);
  }).join("\n");
}

function signalingHarness(random) {
  const sockets = [], timers = [];
  class Socket {
    static OPEN = 1; static CONNECTING = 0;
    constructor(url) { this.url = url; this.readyState = 0; sockets.push(this); }
    close() { this.readyState = 3; }
  }
  const context = createSessionContext({ WebSocket: Socket, URL, recovery: recoveryStub(),
    inputStatusReason: null, latestInputStatus: null, terminalSignalingFailure: false, reconnectTimer: null, ws: null,
    signalingGeneration: 0, signalingRegistered: false, authenticatedSession: null, hudState: {}, hud: { classList: { add() {} } },
    signalingSocketUrl: () => "wss://test.invalid/ws", debugTrace() {}, viewerLog() {}, recordSignalingError() {},
    restartPeer() {}, cleanupPeer() {}, handleSignal() {}, clearTimeout() {},
    connectTimer: null, pendingConnect: null, signalingBackoffMs: 0,
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; } });
  vm.runInContext(receiverFunctions("noteSessionAuthenticated"), context);
  attachSignaling(context, { random });
  context.reconnectSignaling("startup");
  const lose = async () => {
    const dead = sockets.at(-1);
    dead.readyState = 3;
    dead.onclose({ code: 1006 });
    for (let i = 0; i < 4; i++) await Promise.resolve();
    const timer = timers.at(-1);
    timer.fn();
    return timer.delay;
  };
  return { context, lose };
}

test("signaling reconnects back off to the cap with jitter and restart from the base after authentication", async () => {
  const bases = [V4_BACKOFF_MIN_MS, 2 * V4_BACKOFF_MIN_MS, 4 * V4_BACKOFF_MIN_MS, 8 * V4_BACKOFF_MIN_MS, 16 * V4_BACKOFF_MIN_MS, V4_BACKOFF_MAX_MS, V4_BACKOFF_MAX_MS];
  for (const [random, side] of [[() => 0, 1 - V4_BACKOFF_JITTER_FRACTION], [() => 0.999999, 1 + V4_BACKOFF_JITTER_FRACTION]]) {
    const h = signalingHarness(random);
    for (const base of bases) assert.equal(await h.lose(), Math.round(base * side));
    vm.runInContext("noteSessionAuthenticated()", h.context);
    assert.equal(await h.lose(), Math.round(V4_BACKOFF_MIN_MS * side));
  }
});
