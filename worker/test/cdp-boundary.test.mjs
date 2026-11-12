import { readClientSource } from "./helpers/client-source.mjs";
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import { createCompositionCommitGate } from "../public/composition-gate.js";

test("receiver diagnostics expose current host capabilities, capture and clock", () => {
  const client = readClientSource();
  assert.match(client, /hostCapabilities:\s*\{\s*\.\.\.hostCapabilities,\s*current:\s*hostCapabilitiesGeneration === signalingGeneration/);
  assert.match(client, /captureFrame\(\)\s*\{[\s\S]*?toDataURL\("image\/png"\)/);
  assert.match(client, /clock:\s*\{\s*performanceMs:\s*snapshotAtMs,\s*epochMs:\s*snapshotAtEpochMs\s*\}/);
});

test("external debug attempts report before module imports and across the WSS presentation chain", () => {
  const client = readClientSource();
  const preModule = client.indexOf("window.__bubbleplayDebugTrace");
  const module = client.indexOf('<script type="module">');
  assert.ok(preModule >= 0 && preModule < module, "debug reporter must execute before module imports");
  const prelude = client.match(/<script>\s*([\s\S]*?)<\/script>\s*<script type="module">/)?.[1];
  assert.ok(prelude, "the inline bootstrap prelude must be extracted from the client");
  assert.doesNotThrow(() => new Function(prelude));
  assert.doesNotMatch(client, /__diag/, "the dead /__diag/client post is gone");
  assert.match(client, /window\.BPLog\?\.info\("trace"/);
  assert.match(client, /<script src="\/dk-log\.js"><\/script>[\s\S]*window\.__bubbleplayDebugTrace/, "the logger loads before the bootstrap");
  assert.match(client, /debugAttempt/);
  assert.match(client, /trace\("bootstrap-inline"/);
  for (const stage of [
    "module-ready", "signaling-construct", "signaling-open",
    "signaling-hello-ok", "media-v4-open", "media-first-packet",
    "decoder-configured", "decoder-first-output", "first-presented",
  ]) assert.match(client, new RegExp(`debugTrace\\(\"${stage}\"`));
});

test("a normal page load creates and reports its own diagnostic attempt", () => {
  const client = readClientSource();
  const prelude = client.match(/<script>\s*([\s\S]*?)<\/script>\s*<script type="module">/)?.[1];
  assert.ok(prelude, "the inline bootstrap prelude must be extracted from the client");
  const generatedAttempt = "11111111-2222-4333-8444-555555555555";
  const lines = [];
  const windowObject = { BPLog: { info: (...args) => lines.push(args) } };

  new Function("location", "crypto", "window", "URLSearchParams", prelude)(
    { search: "", pathname: "/" },
    { randomUUID: () => generatedAttempt },
    windowObject,
    URLSearchParams,
  );

  assert.equal(windowObject.__bubbleplayDebugTrace?.attemptId, generatedAttempt);
  assert.equal(lines.length, 1);
  assert.deepEqual(lines[0].slice(0, 2), ["trace", "bootstrap-inline"]);
  assert.equal(lines[0][2].attempt, generatedAttempt);
  assert.equal(lines[0][2].seq, 1);
  assert.equal(lines[0][2].navigation, null, "no performance entries: navigation is null");
  for (const type of ["reload", "navigate", "back_forward"]) {
    const typed = [];
    new Function("location", "crypto", "window", "URLSearchParams", prelude)(
      { search: "", pathname: "/" }, { randomUUID: () => generatedAttempt },
      { BPLog: { info: (...args) => typed.push(args) },
        performance: { getEntriesByType: kind => (kind === "navigation" ? [{ type }] : []) } },
      URLSearchParams);
    assert.equal(typed[0][2].navigation, type);
  }
  // A throwing Performance API never breaks the page or the trace.
  const broken = [];
  assert.doesNotThrow(() => new Function("location", "crypto", "window", "URLSearchParams", prelude)(
    { search: "", pathname: "/" }, { randomUUID: () => generatedAttempt },
    { BPLog: { info: (...args) => broken.push(args) }, performance: { getEntriesByType() { throw new Error("no"); } } },
    URLSearchParams));
  assert.equal(broken[0][2].navigation, null);
  // The client logger's redactor keeps the value readable in the stored line.
  const context = vm.createContext({ console: { warn() {}, error() {} }, addEventListener() {}, setTimeout: () => 1,
    Date, JSON, Error, Promise, Set, Math, Number, String, Array, Object, RegExp, indexedDB: undefined });
  context.globalThis = context;
  vm.runInContext(readFileSync(new URL("../public/dk-log.js", import.meta.url), "utf8"), context);
  const raw = context.BPLogCreate({ indexedDB: undefined, setTimeout: () => 1, console: { warn() {}, error() {} } });
  for (const type of ["navigate", "reload", "back_forward", "prerender"]) {
    raw.info("trace", "bootstrap-inline", { attempt: generatedAttempt, navigation: type });
    assert.equal(JSON.parse(JSON.stringify(raw.recent(1)))[0].fields.navigation, type);
  }
  // Without the logger script the page still starts.
  assert.doesNotThrow(() => new Function("location", "crypto", "window", "URLSearchParams", prelude)(
    { search: "", pathname: "/" }, { randomUUID: () => generatedAttempt }, {}, URLSearchParams));
  assert.match(client, /attempt: diagnosticAttemptId,/);
});

test("forced WSS debugging preserves the original failure instead of falling back", () => {
  const client = readClientSource();
  const recovery = client.match(/function recoverWssStartup\(reason\) \{([\s\S]*?)\n\}/)?.[1];
  assert.ok(recovery, "recoverWssStartup must be extracted from the client");
  assert.match(recovery, /requestedTransport === TRANSPORT\.WSS[\s\S]*recoverNetworkRoute/);
  const forced = recovery.match(/if \(requestedTransport === TRANSPORT\.WSS\) \{([\s\S]*?)\n  \}/)?.[1];
  assert.ok(forced, "the forced WSS branch must be extracted from recoverWssStartup");
  assert.doesNotMatch(forced, /wssSelected\s*=\s*false|wss-fallback-webrtc/);
});

test("modern transform keeps canvas/decode/render in the DedicatedWorker", () => {
  const client = readClientSource();
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

test("automatic WSS fallback releases the WebRTC video sink before reclaiming its canvas", () => {
  const client = readClientSource();
  assert.match(client, /function releaseWebRtcVideoSink\(\)[\s\S]*encodedWorker\.terminate\(\)[\s\S]*replaceTransferredCanvas\(\)/);
  assert.match(client, /function requestWssMediaStart\(\)[\s\S]*releaseWebRtcVideoSink\(\);[\s\S]*ensureMainCanvas\(\);/);
  assert.match(client, /const updateDimensions = \(\) => \{\s*if \(generation !== peerGeneration \|\| activeVideo !== video \|\| !nativePresentation\) return;/);
  const release = client.match(/function releaseWebRtcVideoSink\(\) \{([\s\S]*?)\n\}/)?.[1];
  assert.ok(release, "releaseWebRtcVideoSink must be extracted from the client");
  assert.doesNotMatch(release, /\bpc\?*\.close|\bdc\?*\.close|\bmotionDc\?*\.close/);
});

test("WSS diagnostics count media bytes and packets and reset them atomically", () => {
  const client = readClientSource();
  assert.match(client, /wssMediaReceivedBytes\s*\+=\s*data\.byteLength/);
  assert.match(client, /wssMediaReceivedPackets\+\+/);
  assert.match(client, /wssMedia:\s*\{[\s\S]*receivedBytes:\s*wssMediaReceivedBytes[\s\S]*receivedPackets:\s*wssMediaReceivedPackets/);
  assert.match(client, /resetPresentationCounters\(\)[\s\S]*wssMediaReceivedBytes\s*=\s*0[\s\S]*wssMediaReceivedPackets\s*=\s*0/);
});

test("WSS presentation diagnostics prove immediate draws and expose every RAF coalescing path", () => {
  const client = readClientSource();
  assert.match(client, /FramePresentationTelemetry/);
  assert.match(client, /const framePresentationTelemetry = new FramePresentationTelemetry\(\)/);
  assert.match(client, /presentWssFrameImmediately\([\s\S]*?framePresentationTelemetry[\s\S]*?\)/);
  assert.match(client, /framePresentationTelemetry\.noteImmediatePresented\(\)/);
  assert.match(client, /framePresentationTelemetry\.noteRafEnqueued\(\)/);
  assert.match(client, /framePresentationTelemetry\.noteRafCoalesced\(\)/);
  assert.match(client, /framePresentationTelemetry\.noteRafLateDrop\(\)/);
  assert.match(client, /presentation:\s*framePresentationTelemetry\.snapshot\(\)/);
  assert.match(client, /resetPresentationCounters\(\)[\s\S]*framePresentationTelemetry\.reset\(\)/);
});

test("native video is the default path and WebCodecs is opt-in", () => {
  const client = readClientSource();
  assert.match(client, /const useWebCodecs = params\.get\("decoderPath"\) === "webcodecs";/);
  assert.match(client, /if \(!useWebCodecs\) \{\s*startNativeVideo\(ev\.track(?:, generation)?\);\s*return;/);
  assert.match(client, /if \(useWebCodecs\) peerOptions\.encodedInsertableStreams = true;/);
  assert.match(client, /video\.requestVideoFrameCallback\(onVideoFrame\)/);
  assert.match(client, /nativePresentation:/);
  assert.doesNotMatch(client, /encodedInsertableStreams:\s*true/);
});

test("native video keeps an explicit input overlay and diagnostics count presented frames", () => {
  const client = readClientSource();
  assert.match(client, /#stage\.native-video-input #screen/);
  assert.match(client, /stage\.classList\.add\("native-video-input"\)/);
  assert.match(client, /z-index:1;pointer-events:none/);
  assert.match(client, /function videoContentRect\(video\)/);
  assert.match(client, /const r = videoContentRect\(activeVideo \|\| screenMedia\(\)\);/);
  // The canvas scales up to the stage like the native video does.
  assert.match(client, /canvas \{\s*width: 100%; height: 100%;\s*object-fit: contain;/);
  assert.match(client, /framesDrawn = nativePresentation\.framesPresented/);
  assert.match(client, /renderedTimestamp: lastRenderedTimestamp \?\? \(nativeFramesPresented > 0 \? nativeFramesPresented : null\)/);
});

test("responsive product shell keeps normal controls compact and diagnostics advanced-only", () => {
  const client = readClientSource();
  assert.match(client, /id="connection-status"/);
  assert.match(client, /id="session-strip"/);
  assert.match(client, /data-ui-action="keyboard"/);
  assert.match(client, /data-ui-action="monitors"/);
  assert.match(client, /data-ui-action="shortcuts"/);
  assert.match(client, /data-ui-action="settings"/);
  assert.match(client, /data-ui-action="fullscreen"/);
  assert.match(client, /data-ui-action="more"/);
  assert.match(client, /id="settings-panel"[^>]*role="dialog"/);
  // Desktop zoom buttons and the desktop keyboard button are gone; touch zoom stays.
  assert.doesNotMatch(client, /id="zoom-in"|id="zoom-out"/);
  assert.match(client, /class="strip-item only-touch" data-ui-action="keyboard"/);
  assert.match(client, /id="zoom-reset"/);
  assert.match(client, /id="diagnostics-output"/);
  assert.match(client, /function setDiagnosticsVisible\(visible\)/);
  assert.match(client, /params\.get\("diag"\) === "1"/);
  assert.match(client, /id="secure-desktop-status"/);
  assert.match(client, /Windows secure desktop/);
  assert.match(client, /desktopKind === "secure"/);
  assert.match(client, /keyboardInput\.setAttribute\("autocorrect", secureDesktop \? "off" : "on"\)/);
  assert.match(client, /secureTextUnits/);
  // Touch order follows the Android quick-action bar.
  assert.match(client, /data-ui-action="keyboard"[\s\S]*data-ui-action="monitors"[\s\S]*data-ui-action="pointer"[\s\S]*data-ui-action="shortcuts"[\s\S]*data-ui-action="keys"[\s\S]*data-ui-action="arrows"[\s\S]*data-ui-action="more"/);
});

test("session strip is docked above the stream and hides only in desktop fullscreen", () => {
  const client = readClientSource();
  const styles = readFileSync(new URL("../public/bubbleplay.css", import.meta.url), "utf8");
  assert.match(styles, /--strip-height:\s*32px/);
  assert.match(styles, /body\.session-page #input-surface \{ top: var\(--strip-top\); bottom: var\(--strip-bottom\); \}/);
  assert.match(styles, /body\.is-fullscreen \{ --strip-top: 0px; \}/);
  assert.match(client, /id="strip-handle"/);
  assert.match(client, /createEdgeReveal\(\{ delayMs: 300/);
  assert.match(client, /function setStripPeek\(on\)/);
  // Strip buttons never take keyboard focus from the remote input surface.
  assert.match(client, /button\.addEventListener\("mousedown", e => e\.preventDefault\(\)\)/);
  assert.doesNotMatch(client, /id="chrome-reveal"|id="control-sheet"|setChromeDockOpen/);
  assert.doesNotMatch(styles, /--top-seam-height|#control-sheet/);
});

test("first-frame startup uses one bounded retry and clears on native or decoded presentation", () => {
  const client = readClientSource();
  assert.match(client, /id="desktop-state"[^>]*data-state="starting"/);
  assert.match(client, /id="first-frame-retry"/);
  assert.match(client, /type: "first-frame"/);
  assert.match(client, /phase: "request"/);
  assert.match(client, /phase: "presented"/);
  assert.match(client, /phase === "admitted"/);
  assert.match(client, /phase === "failed"/);
  assert.match(client, /phase === "failed"[\s\S]*firstFrameHandshake\.presented[\s\S]*first-frame-failure-ignored[\s\S]*return;/);
  assert.match(client, /function resetPresentationCounters\(\)/);
  assert.match(client, /function cleanupPeer\(\)[\s\S]*resetPresentationCounters\(\)/);
  assert.match(client, /recoverWssStartup/);
  assert.match(client, /monitorTransition/);
  assert.match(client, /desktopTransitionPlan\(\{ reason: message\.reason, state: message\.state, usingWss: usingWssMedia, alreadyRendering, rtcInPlace: rtcInPlaceTransitionCapable\(\) \}\)/);
  assert.match(client, /function startFirstFrameHandshake\(reason, forceNew = false\)/);
  assert.match(client, /firstFrameHandshake.retries < 1/);
  assert.match(client, /}, 5_000\)/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null, presentedViewportRevision = null, proof = null\)/);
  assert.match(client, /video\.requestVideoFrameCallback/);
  assert.match(client, /markFirstFramePresented\(null, null, \{ rtpTimestamp \}\);/);
  assert.match(client, /lastDecodedDimensions/);
});

test("BubbleBuffer edge instrument replaces purple glass and exposes adaptive session controls", () => {
  const client = readClientSource();
  const styles = readFileSync(new URL("../public/bubbleplay.css", import.meta.url), "utf8");

  assert.match(client, /href="\/bubbleplay\.css"/);
  assert.match(client, /class="session-page/);
  assert.match(client, /id="context-hint"/);
  assert.match(client, /id="switch-pc"/);
  assert.match(client, /id="lock-key"/);
  assert.match(client, /id="forget-browser"/);
  assert.match(client, /function syncShellMode\(\)/);
  assert.match(client, /classList\.toggle\("mode-game"/);
  assert.match(client, /classList\.add\("has-presented-frame"\)/);
  assert.match(styles, /--bright-apricot:\s*#F0A064/i);
  assert.match(styles, /@font-face[\s\S]*atkinson-hyperlegible-next\.woff2/);
  // Touch layout docks the bar at the bottom with 48px targets (session-ui.md, Web touch).
  assert.match(styles, /@media \(pointer: coarse\)/);
  // The session strip stays docked; it never auto-hides (session-ui.md).
  assert.doesNotMatch(styles, /chrome-idle/);
  assert.doesNotMatch(client, /chrome-idle/);
  assert.doesNotMatch(styles, /#7c3aed|#9b6cff|backdrop-filter|linear-gradient|radial-gradient/i);
});

test("client starts a new signaling and peer epoch after relay loss or peer departure", () => {
  const client = readClientSource();
  assert.match(client, /function reconnectSignaling\(reason\)/);
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  const reconnect = client.match(/function reconnectSignaling\(reason\) \{[\s\S]*?\n\}/)?.[0];
  assert.ok(reconnect);
  assert.match(reconnect, /socket\.onclose = event => \{[\s\S]*receiveQueue = receiveQueue\.then\(\(\) => handleClose\(event\)\);/);
  assert.match(reconnect, /const handleClose = event => \{[\s\S]*authenticatedSession\?\.close\(\);[\s\S]*cleanupPeer\(\);[\s\S]*restartPeer\("ws-closed", false\);\s*reconnectSignaling\("ws-closed"\);/);
  assert.match(client, /const generation = \+\+peerGeneration/);
  assert.match(client, /pendingRemoteIce/);
});

test("browser signaling sends the signed viewer hello and never stores an application secret", () => {
  const client = readClientSource();
  assert.match(client, /sendRelay\(viewerHello\(\{ identity: sessionIdentity, turn: wantsTurn\(params\) \}\)\)/);
  assert.doesNotMatch(client, /bp_token|BubblePlay token|hello", token/);
  assert.doesNotMatch(client, /localStorage[^\n]*(privateKey|sessionIdentity)/);
});

test("WSS fallback negotiates the v4 media socket and keeps the control surface", () => {
  const client = readClientSource();
  assert.match(client, /parseTransport\(params\)/);
  assert.match(client, /parseQualityPreset\(params, qualityStorageView\(settings\)\)/);
  assert.match(client, /settings\.quality = qualityPreset;\s*persistSettings\(\);/);
  assert.match(client, /<select id="quality"/);
  assert.match(client, /action: "quality"/);
  assert.match(client, /quality: qualityPreset === "auto" \? "high" : qualityPreset, \.\.\.\(audioDecodable \? \{ audio: 1 \} : \{\}\) \}\);/);
  assert.match(client, /streamMode: streamModeValue\(\)/);
  assert.match(client, /<option value="auto">Auto<\/option>/);
  assert.match(client, /function mediaSocketUrl\(\)/);
  assert.match(client, /role: "media-viewer"/);
  assert.match(client, /hev1\.1\.6\.L120\.B0/);
  assert.match(client, /media-control/);
  assert.match(client, /fullFrameDiagnostic/);
  assert.match(client, /diagnostic-full-frame/);
  assert.match(client, /clientBuild/);
  assert.match(client, /mediaGeneration/);
  assert.match(client, /<canvas id="screen"><\/canvas><canvas id="sharp-screen"><\/canvas>/);
  assert.match(client, /function beginWssRecovery\(reason, requestId = null\)/);
  assert.match(client, /wssRecoveryPending/);
  assert.match(client, /wss-recovery-keyframe/);
  assert.match(client, /case "desktop-state"/);
  assert.match(client, /case "monitor-list"/);
  assert.match(client, /action: "monitor"/);
  assert.match(client, /id="session-strip"/);
  assert.match(client, /id="keyboard-input"/);
  assert.match(client, /reconcileTextEdit/);
  assert.match(client, /let keyboardShadow = ""/);
  assert.match(client, /compositionstart/);
  assert.match(client, /Applied correction/);
  assert.doesNotMatch(client, /keyboardInput\.value = "";\s*\}\);/);
  assert.match(client, /data-vk="162"/);
  assert.match(client, /createPopoverController\(\{/);
  assert.match(client, /desktop-unavailable/);
  assert.match(client, /if \(!desktopAvailable \|\| desktopAwaitingFrame\) return false/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null, presentedViewportRevision = null, proof = null\)[\s\S]*desktopAwaitingFrame = false[\s\S]*renderDesktopState\(\);/);
  assert.doesNotMatch(client, /sharpZoomEnabled\s*&&\s*pointerMode\s*===\s*"desktop"/);
  assert.match(client, /function requestWssMediaStart\(\) \{\s*if \(usingWssMedia\) return;\s*if \(!authenticatedSession\?\.controlReady \|\| !signalingTicket\) return;\s*if \(!mediaEpoch\)[\s\S]*ensureMainCanvas\(\);/);
  assert.match(client, /function requestWssMediaStart\(\)[\s\S]*stage\.classList\.remove\("native-video-input"\)/);
  assert.match(client, /function recoverNetworkRoute\(reason\)/);
  assert.match(client, /addEventListener\("online", \(\) => recoverNetworkRoute\("online"\)\)/);
  assert.match(client, /function probeNetworkRouteChange\(\)/);
  assert.match(client, /routeConnection\?\.addEventListener\("change", probeNetworkRouteChange\)/);
  assert.doesNotMatch(client, /routeConnection\?\.addEventListener\("change", \(\) => recoverNetworkRoute/);
  assert.match(client, /createRtcStartupFallback\(\{[\s\S]*?getStats: \(\) => candidate\.getStats\(\),[\s\S]*?onFallback: \(\) => switchToWss\("no-rtp"\)/);
  assert.doesNotMatch(client, /switchToWss\("ice-timeout"\)/);
  assert.match(client, /2_000/);
  assert.doesNotMatch(client, /localStorage\.setItem\([^\n]*codec/);
  assert.match(client, /const due = usingWssMedia \? now : latencyGuard\.expectedWall\(ts\)/);
  assert.match(client, /wssLatestFrameTrace/);
  assert.match(client, /wssPresentedFrameTraces/);
  assert.match(client, /frameTraceHistory/);
  assert.match(client, /wssPresentedSequence\s*=\s*metadata\.sequence\s*>>>\s*0/);
  assert.doesNotMatch(client, /wssPresentedSequence\s*=\s*Math\.max\(/);
  assert.match(client, /decodeQueuedAtMs/);
  assert.match(client, /decodedAtMs/);
  assert.match(client, /presentedAtMs/);
});



test("QoS polling continues after ICE reaches completed", () => {
  const client = readClientSource();
  assert.match(client, /\["connected",\s*"completed"\]\.includes\(pc\.iceConnectionState\)/);
  assert.doesNotMatch(client, /pc\.iceConnectionState\s*!==\s*"connected"/);
});

test("worker policy closure is terminal while transient websocket closure reconnects", () => {
  const client = readClientSource();
  assert.match(client, /shouldReconnectAfterClose\(event\.code, terminalSignalingFailure\)/);
  assert.match(client, /terminalSignalingFailure/);
  assert.match(client, /reconnectSignaling\("ws-closed"\)/);
});

test("client applies validated ephemeral ICE configuration and keeps route diagnostics private", () => {
  const client = readClientSource();
  assert.match(client, /from "\/turn\.js"/);
  assert.match(client, /currentIceServers = validateIceServers\(msg\.iceServers\);/);
  assert.ok(client.includes('mediaEpoch = typeof msg.mediaEpoch === "string" && /^[\\x20-\\x7e]{1,64}$/.test(msg.mediaEpoch) ? msg.mediaEpoch : null;'),
    "the admitted epoch must be a bounded printable ASCII string, preserved without numeric conversion");
  assert.match(client, /onAuthenticated: \(\) => \{\s*if \(!current\(\)\) return;[\s\S]*?if \(wssSelected\) requestWssMediaStart\(\);\s*else startPeer\(peerGeneration\);/);
  assert.match(client, /iceServers: currentIceServers,\s*iceTransportPolicy,/);
  assert.match(client, /hudState\.route = deriveCandidateRoute\(latestRtcProfile\);/);
  assert.match(client, /hudState\.route = "…";/);
});

test("diagnostic snapshots expose the latest validated host benchmark sample", () => {
  const client = readClientSource();
  assert.match(client, /let latestBenchStatus = null/);
  assert.match(client, /case "bench-status":[\s\S]*applyBenchStatus\(msg\)/);
  assert.match(client, /function applyBenchStatus\(status\)[\s\S]*latestBenchStatus = status/);
  assert.match(client, /snapshot\(\)[\s\S]*benchStatus: latestBenchStatus/);
});

test("WSS HEVC decoder tracks Main versus Main10 and probes Main10 without H264 fallback", () => {
  const client = readClientSource();
  assert.match(client, /hev1\.1\.6\.L120\.B0/);
  assert.match(client, /hev1\.2\.4\.L120\.B0/);
  assert.match(client, /latestBenchStatus\?\.effective\?\.encoder\?\.main10/);
  assert.match(client, /main10[\s\S]*configure-unsupported/);
});

test("WSS hybrid presentation parses before decode and composites only after the base frame", () => {
  const client = readClientSource();
  const activeHybridMode = client.match(
    /function activeWssHybridMode\(\) \{[\s\S]*?\n\}/,
  )?.[0];
  assert.match(client, /prepareHybridWssFrame/);
  assert.match(client, /new HybridTileCache/);
  assert.match(client, /new HevcTileCache/);
  assert.match(client, /probeHevcTileDecoderSupport/);
  assert.ok(activeHybridMode);
  assert.match(activeHybridMode, /codecSession\.codec === "hevc"/);
  assert.match(client, /function decodeWssFrame\(frame\)[\s\S]*prepareHybridWssFrame/);
  assert.match(client, /viewerLog\("info", "hybrid-frame-activity"/);
  assert.match(client, /viewerLog\("warn", "wss-recovery-reset"/);
  assert.match(client, /viewerLog\("error", "decoder-decode-throw", \{\s*message:/);
  assert.match(client, /function drawFrame\(frame[\s\S]*drawBaseThenHybrid/);
  assert.match(client, /stopWssMedia\(\)[\s\S]*clearWssHybridCaches\(\)/);
  assert.match(client, /resyncWssDecoder\(reason\)[\s\S]*clearWssHybridCaches\(\)/);
  assert.match(client, /wssMedia:\s*\{[\s\S]*hybrid:\s*wssHybridSnapshot\(\)/);
});

test("peer departure recycles signaling before another peer offer", () => {
  const client = readClientSource();
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  assert.match(client, /function recycleSignaling\(reason\)/);
});

test("intentional signaling recycle retains its initiator before invalidating the old epoch", () => {
  const client = readClientSource();
  const recycle = client.match(/function recycleSignaling\(reason\) \{([\s\S]*?)\n\}/)?.[1];
  assert.ok(recycle, "recycleSignaling must be extracted from the client");
  assert.match(recycle, /createSignalingRecycleDiagnostic\(\{/);
  assert.match(recycle, /reason,[\s\S]*signalingGeneration,[\s\S]*signalingRegistered,/);
  assert.match(recycle, /socketReadyState:\s*ws\?\.readyState\s*\?\?\s*null/);
  assert.match(recycle, /usingWssMedia,[\s\S]*mediaSocketOpen:/);
  assert.match(recycle, /mediaGeneration:[\s\S]*reconstructed:[\s\S]*presented:[\s\S]*receivedPackets:/);
  assert.match(recycle, /lastSignalingRecycle\s*=\s*\{\s*\.\.\.diagnostic,\s*atMs:\s*Date\.now\(\)\s*\}/);
  const emittedAt = recycle.indexOf('viewerLog("warn", "signaling-recycle", diagnostic)');
  const invalidatedAt = recycle.indexOf("ws = null");
  assert.ok(emittedAt >= 0 && invalidatedAt > emittedAt,
    "the diagnostic must use the still-open old signaling socket before it is invalidated");
  assert.match(client, /signaling:\s*\{[\s\S]*lastRecycle:\s*lastSignalingRecycle\s*\?\s*\{\s*\.\.\.lastSignalingRecycle\s*\}\s*:\s*null/);
});

test("predicted cursor repaints through one follow boundary and has a real black outline", () => {
  const client = readClientSource();
  assert.match(client, /<div id="touch-cursor"[^>]*>\s*<svg[\s\S]*?<path[^>]*stroke="black"/);
  assert.match(client, /function updateRemoteCursor\(update\)[\s\S]*update\(remoteCursor\);[\s\S]*centerCameraOnCursor\(\);\s*renderViewport\(\);/);
  assert.match(client, /function centerCameraOnCursor\(\)[\s\S]*viewport\.centerOnNormalized\(remoteCursor\.snapshot\(\)\)/);
  assert.match(client, /updateRemoteCursor\(cursor\s*=>\s*cursor\.moveScreen\(/);
  assert.match(client, /updateRemoteCursor\(cursor\s*=>\s*cursor\.setNormalized\(/);
});

test("mobile press actions atomically warp reliable control to the overlay cursor", () => {
  const client = readClientSource();
  assert.match(client, /function setTouchButtonsAtCursor\(mask\)[\s\S]*input\.buttonsAt\(point\.x,point\.y,mask\)/);
  assert.match(client, /action\.type === "tap"[\s\S]*setTouchButtonsAtCursor\(1\); setTouchButtons\(0\)/);
  assert.match(client, /action\.type === "drag-start"[\s\S]*setTouchButtonsAtCursor\(1\)/);
  assert.match(client, /action\.type === "right-tap"[\s\S]*setTouchButtonsAtCursor\(2\); setTouchButtons\(0\)/);
});

test("mobile desktop pointer uses absolute targets and authoritative host reconciliation", () => {
  const client = readClientSource();
  assert.match(client, /Work · absolute pointer/);
  assert.match(client, /new CursorReconciler\(remoteCursor\)/);
  assert.match(client, /channel\.onmessage[\s\S]*handleCursorState/);
  assert.match(client, /motionChannel\.onmessage[\s\S]*handleCursorState/);
  assert.match(client, /function queueTouchAbsolute[\s\S]*input\.absolute/);
  assert.match(client, /pointerMode === "desktop"[\s\S]*queueTouchAbsolute\(point\.x,point\.y\)/);
  assert.match(client, /action\.type === "motion-end"[\s\S]*setTouchButtonsAtCursor\(touchButtons\)/);
  assert.match(client, /pointerMode === "game"[\s\S]*queueTouchMotion/);
  assert.match(client, /shouldCorrectCursorDrift/);
  // Capture cursor is always available on desktop: it switches to Game mode and locks the pointer.
  assert.match(client, /document\.getElementById\("capture"\)\.onclick = \(\) => \{[\s\S]*if \(pointerMode !== "game"\) setPointerMode\("game"\);\s*requestPointerLock\(\);/);
});

test("desktop input suspension is enforced at the final browser transport boundary", () => {
  const client = readClientSource();
  assert.match(client, /function sendInput\(data, channel\) \{\s*if \(!authenticatedSession\?\.mediaReady\) return false;\s*if \(desktopInputState !== "enabled"\) return false;/);
  assert.match(client, /function inputControlState\(\) \{\s*if \(!authenticatedSession\?\.mediaReady\) return "down";\s*if \(desktopInputState !== "enabled" \|\| !desktopAvailable\) return "down";/);
  assert.match(client, /if \(desktopInputState !== "enabled"\) releaseInput\(false\);/);
});

test("browser diagnostics retain host-side applied input evidence", () => {
  const client = readClientSource();
  assert.match(client, /case "input-status"/);
  assert.match(client, /latestInputStatus = msg/);
  assert.match(client, /host-input-application-failed/);
  assert.match(client, /appliedEvents.*requestedEvents/);
});

test("desktop source transitions preserve the last frame without authorizing stale input", () => {
  const client = readClientSource();
  assert.match(client, /const sourceTransition = transitionPlan\?\.kind === "source";/);
  assert.doesNotMatch(client, /sourceTransition = message\.state === "active" && \(secureDesktop \|\|/);
  // Monitor switches stay in place when the frame generation can be proven and keep held input;
  // otherwise the WebRTC peer is recycled with input released.
  const monitorBranch = client.match(/if \(monitorTransition && alreadyRendering\) \{[\s\S]*?\r?\n  \}/)?.[0];
  assert.ok(monitorBranch, "the monitor-transition branch must be extracted from the client");
  assert.match(monitorBranch, /desktopAwaitingFrame = true;[\s\S]*suppressDesktopTransitionOverlay = true;[\s\S]*inputTransitionHold = true;[\s\S]*startFirstFrameHandshake\("monitor-transition", true\);/);
  assert.match(monitorBranch, /if \(transitionPlan\.action === "restart-peer"\) \{[\s\S]*releaseInput\(false\);[\s\S]*restartRtcForTransition\("monitor-transition"\);\s*return;\s*\}\s*inputTransitionHold = true;/);
  assert.match(client, /const RTC_IN_PLACE_TRANSITIONS = \["monitor-transition", "desktop-source-transition"\];/);
  assert.match(client, /function recoverRtcInPlaceTransition\(reason\) \{[\s\S]*RTC_IN_PLACE_TRANSITIONS\.includes\(firstFrameHandshake\.reason\)[\s\S]*restartPeerInNewEpoch/);
  // Admission without a provable RTP timestamp falls back to the peer restart.
  assert.match(client, /const plan = rtcAdmissionGate\(\{[\s\S]*admittedRtpTimestamp: message\.rtpTimestamp[\s\S]*if \(plan\.action === "restart-peer"\) \{\s*recoverRtcInPlaceTransition\(plan\.reason\);\s*return;/);
  assert.match(client, /requireAdmission && !usingWssMedia\s*&& !rtcGateOpens\(firstFrameHandshake\.rtpGate, proof\)\) return false;/);
  assert.match(client, /if \(inputTransitionHold\) \{\s*inputTransitionHold = false;\s*input\.resumeAfterTransition\(\);/);
  assert.match(client, /if \(sourceTransition && alreadyRendering\) \{[\s\S]*desktopAwaitingFrame = true;[\s\S]*startFirstFrameHandshake\("desktop-source-transition", true\);[\s\S]*return;\s*\}/);
  assert.match(client, /if \(transitionPlan\.action === "restart-peer"\) \{\s*restartRtcForTransition\("desktop-source-transition"\);\s*return;/);
  assert.match(client, /function restartRtcForTransition\(transition\) \{\s*rtcTransitionRestartPending = true;\s*restartPeerInNewEpoch\(\{ recycleSignaling, restartPeer \}, transition\);/);
  assert.match(client, /if \(workerPresented\) markFirstFramePresented\(null, null, \{ gatePassed \}\)/);
  assert.match(client, /firstFrameHandshake\.requireAdmission && !usingWssMedia && useWebCodecs[\s\S]*resyncPipeline\("desktop-transition-admitted", firstFrameHandshake\.rtpGate\)/);
  assert.match(client, /if \(!usingWssMedia && firstFrameHandshake\.requireAdmission\) \{[\s\S]*if \(!firstFrameHandshake\.admitted\)[\s\S]*if \(transitionNeedsKeyframe\)[\s\S]*const verdict = transitionKeyframeVerdict\(\{ isKey,[\s\S]*if \(verdict !== \"admit\"\)[\s\S]*if \(verdict === \"unprovable\"\) recoverRtcInPlaceTransition\(\"no-frame-rtp-timestamp\"\);/);
  assert.match(client, /function sendInput\(data, channel\) \{[\s\S]*if \(!desktopAvailable \|\| desktopAwaitingFrame\) return false;/);
  assert.match(client, /firstFrameHandshake\.requireAdmission = forceNew \|\| reason === "desktop-source-transition"/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null, presentedViewportRevision = null, proof = null\)[\s\S]*requireAdmission && !firstFrameHandshake\.admitted[\s\S]*return false/);
  assert.match(client, /presentedGeneration !== firstFrameHandshake\.generation[\s\S]*return false/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null, presentedViewportRevision = null, proof = null\)[\s\S]*desktopAwaitingFrame = false/);
});

test("monitor control uses the signaling epoch and drops stale confirmations", () => {
  const client = readClientSource();
  assert.match(client, /import \{\s*desktopTransitionPlan, monitorRequestAfterRestart, monitorRequestReadiness, restartPeerInNewEpoch,[\s\S]*?\} from "\/monitor-selection\.js"/);
  // Both epoch restarts drop an outstanding confirmation instead of latching
  // monitor selection off for the rest of the page.
  assert.match(client, /function recycleSignaling\(reason\) \{[\s\S]*invalidatePendingMonitorRequest\(reason\);/);
  assert.match(client, /function restartPeer\(reason, reconnect = true\) \{[\s\S]*invalidatePendingMonitorRequest\(reason\);/);
  assert.match(client, /function invalidatePendingMonitorRequest\(reason\) \{[\s\S]*monitorRequestAfterRestart\(monitorSwitchPending, reason\)[\s\S]*monitorSwitchPending = null;/);
  // The monitor control is a signaling message; input data channels are not a
  // precondition.
  assert.match(client, /function requestMonitor\(selector\) \{[\s\S]*monitorRequestReadiness\(\{[\s\S]*signalingRegistered,[\s\S]*\}\);/);
  assert.doesNotMatch(client, /if \(monitorSwitchPending \|\| !inputReady && !usingWssMedia\) return false;/);
  assert.match(client, /monitorSwitchPending,\s*lastMonitorRequest:/);
});

test("an active desktop generation change re-arms the first-frame input gate", () => {
  const client = readClientSource();
  const handler = client.match(/function handleDesktopState\(message\) \{[\s\S]*?\n\}/)?.[0];
  assert.ok(handler, "handleDesktopState must be extracted from the client");
  assert.match(client, /let desktopGeneration = null/);
  assert.match(handler, /const desktopGenerationChanged = Number\.isInteger\(message\.generation\)[\s\S]*message\.generation !== desktopGeneration/);
  assert.match(handler, /desktopGeneration = message\.generation/);
  assert.match(handler, /startFirstFrameHandshake\("desktop-state", desktopStateNeedsNewHandshake\(desktopGenerationChanged\)\)/);
  assert.match(client, /firstFrameHandshake\.requireAdmission = forceNew \|\| reason === "desktop-source-transition" \|\| reason === "monitor-transition"/);
});

test("normal text assistance is restored outside the secure desktop", () => {
  const client = readClientSource();
  assert.match(client, /id="keyboard-input"[^>]*autocomplete="on"[^>]*autocorrect="on"[^>]*spellcheck="true"/);
  assert.match(client, /keyboardInput\.setAttribute\("autocorrect", secureDesktop \? "off" : "on"\)/);
  assert.match(client, /keyboardInput\.setAttribute\("autocomplete", secureDesktop \? "off" : "on"\)/);
  assert.match(client, /keyboardInput\.spellcheck = !secureDesktop/);
});

test("secure Android composition commits through one pending transaction", () => {
  const factory = createCompositionCommitGate;
  const committed = [];
  let scheduled = null;
  const gate = factory(value => committed.push(value), callback => { scheduled = callback; return callback; }, handle => { if (scheduled === handle) scheduled = null; });

  // Android commonly emits compositionend followed by one non-composing
  // input event. That input consumes the pending transaction synchronously;
  // the fallback cannot send the same text a second time.
  assert.equal(gate.stage("é"), true);
  assert.equal(gate.consume("é"), true);
  scheduled?.();
  assert.deepEqual(committed, ["é"]);

  // Browsers which omit the trailing input still commit once via fallback.
  assert.equal(gate.stage("🙂"), true);
  const fallback = scheduled;
  fallback();
  fallback();
  assert.deepEqual(committed, ["é", "🙂"]);
});
