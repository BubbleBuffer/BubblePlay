import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { MEDIA_CONNECT_TIMEOUT_MS } from "../public/recovery.js";
import { TRANSPORT } from "../public/transport.js";

// Behaviour of receiver.js paths that are not modules yet, run in a vm sandbox as the other receiver tests do.
const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replaceAll("\r\n", "\n");

function receiverFunctions(...names) {
  return names.map(name => {
    const declaration = `function ${name}(`;
    const found = source.indexOf(declaration);
    assert.ok(found >= 0, `receiver function ${name}`);
    const start = source.startsWith("async ", found - 6) ? found - 6 : found;
    return source.slice(start, source.indexOf("\n}\n", start) + 3);
  }).join("\n");
}

function peerHarness(requestedTransport) {
  const calls = { constructed: [], wss: [], logged: [] };
  const STUN_FALLBACK = [{ urls: "stun:fallback.test" }];
  class ThrowingPeer { constructor(options) { calls.constructed.push(options); throw new Error("resource limit"); } }
  const context = vm.createContext({ RTCPeerConnection: ThrowingPeer, STUN_FALLBACK, currentIceServers: [{ urls: "turn:x.test" }],
    requestedTransport, TRANSPORT, hudState: {}, hud: { classList: { add() {} } },
    viewerLog: (level, name) => calls.logged.push(`${level}:${name}`), switchToWss: reason => calls.wss.push(reason) });
  vm.runInContext(receiverFunctions("openPeerConnection", "peerConstructionFailed"), context);
  return { context, calls };
}

test("a throwing RTCPeerConnection reaches the WSS fallback in auto mode and the error state when forced", () => {
  const auto = peerHarness(TRANSPORT.AUTO);
  assert.equal(vm.runInContext("openPeerConnection({ iceServers: currentIceServers })", auto.context), null);
  assert.equal(auto.calls.constructed.length, 2, "one retry on the STUN fallback");
  vm.runInContext("peerConstructionFailed()", auto.context);
  assert.deepEqual(auto.calls.wss, ["rtc-construct-failed"]);

  const forced = peerHarness(TRANSPORT.WEBRTC);
  vm.runInContext("peerConstructionFailed()", forced.context);
  assert.deepEqual(forced.calls.wss, []);
  assert.equal(forced.context.hudState.ice, "ice-config-error");
});

test("a media socket that never reaches hello-ok is abandoned after the connect timeout and retried", () => {
  const timers = [], sockets = [], reconnects = [];
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 0; this.closed = false; sockets.push(this); }
    close() { this.closed = true; }
  }
  const context = vm.createContext({ WebSocket: Socket, performance: { now: () => 1 },
    setTimeout: (fn, delay) => { timers.push({ fn, delay }); return timers.length; }, clearTimeout() {},
    usingWssMedia: true, mediaEpoch: "e", signalingTicket: "t", authenticatedSession: { mediaReady: true },
    mediaSocketUrl: () => "wss://relay.test/media-ws", MEDIA_CONNECT_TIMEOUT_MS, viewerLog() {},
    wssV4: { socket: null, socketOpen: false, connectTimer: null, reconnects: 0 },
    scheduleWssV4Reconnect: reason => reconnects.push(reason) });
  vm.runInContext(receiverFunctions("openWssV4Socket"), context);
  vm.runInContext("openWssV4Socket()", context);
  const timeout = timers.find(timer => timer.delay === MEDIA_CONNECT_TIMEOUT_MS);
  assert.ok(timeout, "connect timeout armed");
  timeout.fn();
  assert.equal(sockets[0].closed, true);
  assert.equal(context.wssV4.socket, null);
  assert.deepEqual(reconnects, ["connect-timeout"]);
});
