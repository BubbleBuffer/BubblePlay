import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

test("live collector attaches to a page target and collects rendered and barcode latency", () => {
  const source = readFileSync(new URL("../tools/latency-live.mjs", import.meta.url), "utf8");
  assert.match(source, /webSocketDebuggerUrl/);
  assert.match(source, /json\/list/);
  assert.match(source, /__bubbleplayDiagnostics/);
  assert.match(source, /750/);
  assert.match(source, /glassToGlassSamples/);
  assert.match(source, /timestamp-pattern\.html/);
  assert.doesNotMatch(source, /glassToGlass: \{ status: "unavailable"/);
});

test("modern transform keeps canvas/decode/render in the DedicatedWorker", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const worker = readFileSync(new URL("../public/latency-worker.js", import.meta.url), "utf8");
  assert.match(client, /message\.kind === "control"/);
  assert.match(client, /transferControlToOffscreen\(\)/);
  assert.match(client, /type: "canvas", canvas: offscreen/);
  assert.match(client, /type: "capabilities"/);
  assert.match(worker, /typeof VideoDecoder !== "undefined"/);
  assert.match(worker, /typeof OffscreenCanvas !== "undefined"/);
  assert.match(client, /jitterBufferTarget\s*=\s*0/);
  assert.doesNotMatch(client, /frame-consumed/);
  assert.doesNotMatch(client, /activeReceiver\.sendKeyFrameRequest/);
  assert.match(worker, /transformer\.sendKeyFrameRequest/);
  assert.match(worker, /transformer\.writable\.getWriter/);
  assert.match(worker, /await writer\.write\(value\)/);
  assert.match(worker, /new VideoDecoder/);
  assert.match(worker, /createWorkerTimeline/);
  // Backpressure is the WebCodecs control queue, not frames already accepted
  // by the hardware decoder and awaiting output.
  assert.match(worker, /MAX_DECODE_QUEUE_SIZE\s*=\s*8/);
  assert.match(worker, /decoder\.decodeQueueSize\s*>=\s*MAX_DECODE_QUEUE_SIZE/);
  assert.doesNotMatch(worker, /decodePending\s*>=/);
  assert.match(worker, /MAX_PACING_DELAY_MS\s*=\s*0/);
  assert.match(worker, /recovering:\s*true/);
  assert.match(worker, /setTimeout\(render/);
  assert.doesNotMatch(worker, /kind: "frame"/);
  assert.match(worker, /kind: "control"/);
});

test("native video is the default path and WebCodecs is opt-in", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /const useWebCodecs = params\.get\("decoderPath"\) === "webcodecs";/);
  assert.match(client, /if \(!useWebCodecs\) \{\s*startNativeVideo\(ev\.track(?:, generation)?\);\s*return;/);
  assert.match(client, /if \(useWebCodecs\) peerOptions\.encodedInsertableStreams = true;/);
  assert.match(client, /video\.requestVideoFrameCallback\(onVideoFrame\)/);
  assert.match(client, /nativePresentation:/);
  assert.doesNotMatch(client, /encodedInsertableStreams:\s*true/);
});

test("native video keeps an explicit input overlay and diagnostics count presented frames", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  const collector = readFileSync(new URL("../tools/latency-live.mjs", import.meta.url), "utf8");
  assert.match(client, /#stage\.native-video-input #screen/);
  assert.match(client, /stage\.classList\.add\("native-video-input"\)/);
  assert.match(client, /z-index:1;pointer-events:none/);
  assert.match(client, /function videoContentRect\(video\)/);
  assert.match(client, /const r = activeVideo \? videoContentRect\(activeVideo\) : screen\.getBoundingClientRect\(\);/);
  assert.match(client, /framesDrawn = nativePresentation\.framesPresented/);
  assert.match(client, /renderedTimestamp: lastRenderedTimestamp \?\? \(nativeFramesPresented > 0 \? nativeFramesPresented : null\)/);
  assert.match(collector, /function hasRenderedFrame\(snapshot\)/);
  assert.doesNotMatch(collector, /ready && ready\.ice === "connected" && Number\.isFinite\(ready\.renderedTimestamp\)/);
});

test("client starts a new signaling and peer epoch after relay loss or peer departure", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /function reconnectSignaling\(reason\)/);
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  assert.match(client, /socket\.onclose = \(event\) => \{[\s\S]*reconnectSignaling\("ws-closed"\)/);
  assert.match(client, /const generation = \+\+peerGeneration/);
  assert.match(client, /pendingRemoteIce/);
});

test("QoS polling continues after ICE reaches completed", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /\["connected",\s*"completed"\]\.includes\(pc\.iceConnectionState\)/);
  assert.doesNotMatch(client, /pc\.iceConnectionState\s*!==\s*"connected"/);
});

test("worker policy closure is terminal while transient websocket closure reconnects", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /shouldReconnectAfterClose\(event\.code, terminalSignalingFailure\)/);
  assert.match(client, /terminalSignalingFailure/);
  assert.match(client, /reconnectSignaling\("ws-closed"\)/);
});

test("client applies validated ephemeral ICE configuration and keeps route diagnostics private", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /from "\/turn\.js"/);
  assert.match(client, /currentIceServers = validateIceServers\(msg\.iceServers\);\s*startPeer\(peerGeneration\);/);
  assert.match(client, /iceServers: currentIceServers,\s*iceTransportPolicy,/);
  assert.match(client, /hudState\.route = deriveCandidateRoute\(latestRtcProfile\);/);
  assert.match(client, /hudState\.route = "…";/);
});

test("peer departure recycles signaling before another peer offer", () => {
  const client = readFileSync(new URL("../public/index.html", import.meta.url), "utf8");
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  assert.match(client, /function recycleSignaling\(reason\)/);
});
