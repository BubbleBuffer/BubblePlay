import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// Loads the trickled-ICE helpers from receiver.js into a sandbox, as the other
// receiver tests do: the browser script is not a module.
const source = readFileSync(new URL("../public/receiver.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const start = source.indexOf("function remoteIceInit(");
const end = source.indexOf("\n}\n", source.indexOf("async function addRemoteIce(")) + 3;
const ICE = "candidate:1 1 udp 2130706431 203.0.113.9 4444 typ srflx raddr 0.0.0.0 rport 0";
const SDP = ["v=0", "o=- 1 2 IN IP4 127.0.0.1", "s=-", "t=0 0", "a=group:BUNDLE 0 1",
  "m=video 9 UDP/TLS/RTP/SAVPF 96", "a=mid:0", "m=audio 9 UDP/TLS/RTP/SAVPF 111", "a=mid:1", ""].join("\r\n");

function load(extra = {}) {
  const sandbox = { console, isCurrentPeer: () => true, calls: [], ...extra };
  vm.createContext(sandbox);
  vm.runInContext(source.slice(start, end), sandbox);
  return sandbox;
}

test("trickled host candidates bind to the first remote m-line mid", () => {
  const sandbox = load({ ice: ICE, sdp: SDP });
  const init = JSON.parse(vm.runInContext("JSON.stringify(remoteIceInit(ice, sdp))", sandbox));
  assert.deepEqual(init, { candidate: ICE, sdpMid: "0", sdpMLineIndex: 0 });
});

test("candidates without a remote description fall back to sdpMLineIndex 0", () => {
  const sandbox = load({ ice: ICE });
  const init = JSON.parse(vm.runInContext("JSON.stringify(remoteIceInit(ice, undefined))", sandbox));
  assert.deepEqual(init, { candidate: ICE, sdpMLineIndex: 0 });
});

test("addRemoteIce hands addIceCandidate an init with the m-line, not a bare candidate", async () => {
  const pc = { remoteDescription: { sdp: SDP }, addIceCandidate: async init => {
    sandbox.calls.push(JSON.stringify(init));
  } };
  const sandbox = load({ pc, ICE });
  await vm.runInContext("addRemoteIce(pc, 0, ICE)", sandbox);
  assert.equal(sandbox.calls.length, 1);
  assert.deepEqual(JSON.parse(sandbox.calls[0]), { candidate: ICE, sdpMid: "0", sdpMLineIndex: 0 });
});
