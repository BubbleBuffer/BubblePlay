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
