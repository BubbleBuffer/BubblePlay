import { readClientSource } from "./helpers/client-source.mjs";
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

test("receiver diagnostics identify both index and media asset capabilities", () => {
  const client = readClientSource();
  const media = readFileSync(new URL("../public/media.js", import.meta.url), "utf8");
  assert.match(media, /export const STREAM_LAB_ASSET_BUILD = "wss-v4-bench2-hevc-tiles-bpip4-v16"/);
  assert.match(client, /STREAM_LAB_ASSET_BUILD as MEDIA_ASSET_BUILD/);
  assert.match(client, /const INDEX_ASSET_BUILD = STREAM_LAB_COMPATIBILITY\.receiverBuild/);
  assert.match(client, /compatibility:\s*STREAM_LAB_COMPATIBILITY/);
  assert.match(client, /assetBuild:\s*\{\s*index:\s*INDEX_ASSET_BUILD,\s*media:\s*MEDIA_ASSET_BUILD\s*\}/);
  assert.match(client, /hostCapabilities:\s*\{\s*\.\.\.hostCapabilities,\s*current:\s*hostCapabilitiesGeneration === signalingGeneration/);
  assert.match(client, /captureFrame\(\)\s*\{[\s\S]*?toDataURL\("image\/png"\)/);
  assert.match(client, /clock:\s*\{\s*performanceMs:\s*snapshotAtMs,\s*epochMs:\s*snapshotAtEpochMs\s*\}/);
});

test("external debug attempts report before module imports and across the WSS presentation chain", () => {
  const client = readClientSource();
  const preModule = client.indexOf("window.__bubbleplayDebugTrace");
  const module = client.indexOf('<script type="module">');
  assert.ok(preModule >= 0 && preModule < module, "debug reporter must execute before module imports");
  const prelude = client.match(/<script>\s*([\s\S]*?)<\/script>\s*<script type="module">/)?.[1] || "";
  assert.doesNotThrow(() => new Function(prelude));
  assert.match(client, /fetch\("\/__diag\/client"/);
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
  const prelude = client.match(/<script>\s*([\s\S]*?)<\/script>\s*<script type="module">/)?.[1] || "";
  const generatedAttempt = "11111111-2222-4333-8444-555555555555";
  const requests = [];
  const windowObject = {};

  new Function("location", "crypto", "fetch", "window", "addEventListener", "URLSearchParams", "Date", prelude)(
    { search: "", pathname: "/" },
    { randomUUID: () => generatedAttempt },
    (url, options) => {
      requests.push({ url, options });
      return Promise.resolve({ ok: true });
    },
    windowObject,
    () => {},
    URLSearchParams,
    { now: () => 1_777_777_777_777 },
  );

  assert.equal(windowObject.__bubbleplayDebugTrace?.attemptId, generatedAttempt);
  assert.equal(requests.length, 1);
  assert.equal(requests[0].url, "/__diag/client");
  assert.equal(JSON.parse(requests[0].options.body).attemptId, generatedAttempt);
  assert.match(client, /detail: \{ attemptId: diagnosticAttemptId,/);
});

test("forced WSS debugging preserves the original failure instead of falling back", () => {
  const client = readClientSource();
  const recovery = client.match(/function recoverWssStartup\(reason\) \{([\s\S]*?)\n\}/)?.[1] || "";
  assert.match(recovery, /requestedTransport === TRANSPORT\.WSS[\s\S]*recoverNetworkRoute/);
  const forced = recovery.match(/if \(requestedTransport === TRANSPORT\.WSS\) \{([\s\S]*?)\n  \}/)?.[1] || "";
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
  const release = client.match(/function releaseWebRtcVideoSink\(\) \{([\s\S]*?)\n\}/)?.[1] || "";
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
  assert.match(client, /monitor-switched/);
  assert.match(client, /function startFirstFrameHandshake\(reason, forceNew = false\)/);
  assert.match(client, /firstFrameHandshake.retries < 1/);
  assert.match(client, /}, 5_000\)/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null\)/);
  assert.match(client, /video\.requestVideoFrameCallback/);
  assert.match(client, /markFirstFramePresented\(\);/);
  assert.match(client, /lastDecodedDimensions/);
});

test("login uses the BubblePlay responsive shell while preserving remembered auth", () => {
  const login = readFileSync(new URL("../public/login.html", import.meta.url), "utf8");
  assert.match(login, /class="brand"/);
  assert.match(login, /Welcome back/);
  assert.match(login, /security-note/);
  assert.match(login, /autocomplete="current-password"/);
  assert.match(login, /Remember this browser for 30 days/);
  assert.match(login, /fetch\("\/__auth\/login"/);
});

test("BubbleBuffer edge instrument replaces purple glass and exposes adaptive session controls", () => {
  const client = readClientSource();
  const login = readFileSync(new URL("../public/login.html", import.meta.url), "utf8");
  const styles = readFileSync(new URL("../public/bubbleplay.css", import.meta.url), "utf8");

  assert.match(client, /href="\/bubbleplay\.css"/);
  assert.match(login, /href="\/bubbleplay\.css"/);
  assert.match(client, /class="session-page/);
  assert.match(login, /class="auth-page"/);
  assert.match(login, /class="signal-cut"/);
  assert.match(client, /id="context-hint"/);
  assert.match(client, /id="logout-session"/);
  assert.match(client, /id="forget-browser"/);
  assert.match(client, /function syncShellMode\(\)/);
  assert.match(client, /classList\.toggle\("mode-game"/);
  assert.match(client, /classList\.add\("has-presented-frame"\)/);
  assert.match(client, /endBrowserSession\("\/__auth\/logout", false\)/);
  assert.match(client, /endBrowserSession\("\/__auth\/forget-browser", true\)/);
  assert.match(styles, /--bright-apricot:\s*#F0A064/i);
  assert.match(styles, /@font-face[\s\S]*atkinson-hyperlegible-next\.woff2/);
  // Touch layout docks the bar at the bottom with 48px targets (session-ui.md, Web touch).
  assert.match(styles, /@media \(pointer: coarse\)/);
  // The session strip stays docked; it never auto-hides (session-ui.md).
  assert.doesNotMatch(styles, /chrome-idle/);
  assert.doesNotMatch(client, /chrome-idle/);
  assert.doesNotMatch(styles, /#7c3aed|#9b6cff|backdrop-filter|linear-gradient|radial-gradient/i);
  assert.doesNotMatch(login, /#7c3aed|#9b6cff|backdrop-filter|linear-gradient|radial-gradient/i);
});

test("client starts a new signaling and peer epoch after relay loss or peer departure", () => {
  const client = readClientSource();
  assert.match(client, /function reconnectSignaling\(reason\)/);
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  assert.match(client, /socket\.onclose = \(event\) => \{[\s\S]*reconnectSignaling\("ws-closed"\)/);
  assert.match(client, /const generation = \+\+peerGeneration/);
  assert.match(client, /pendingRemoteIce/);
});

test("browser signaling uses the remembered server session and never stores an application secret", () => {
  const client = readClientSource();
  const login = readFileSync(new URL("../public/login.html", import.meta.url), "utf8");
  assert.match(client, /const labRun = parseLabRun\(params\)/);
  assert.match(client, /labRun \? "\/lab-ws" : "\/ws"/);
  assert.match(client, /labRun \? "\/lab-media-ws" : "\/media-ws"/);
  assert.match(client, /sendSignal\(\{ type: "hello", role: "client", \.\.\.\(wantsTurn\(params\) \? \{ turn: true \} : \{\}\) \}\)/);
  assert.doesNotMatch(client, /bp_token|BubblePlay token|hello", token/);
  assert.match(login, /autocomplete="current-password"/);
  assert.match(login, /Remember this browser for 30 days/);
  assert.match(login, /fetch\("\/__auth\/login"/);
  assert.match(login, /location\.replace\(`\$\{location\.pathname\}\$\{location\.search\}`\)/);
  const inlineScript = login.match(/<script>([\s\S]*?)<\/script>/)?.[1];
  assert.ok(inlineScript);
  assert.doesNotThrow(() => new Function(inlineScript));
});

test("WSS fallback negotiates the v4 media socket and keeps the control surface", () => {
  const client = readClientSource();
  assert.match(client, /parseTransport\(params\)/);
  assert.match(client, /parseQualityPreset\(params, qualityStorageView\(settings\)\)/);
  assert.match(client, /settings\.quality = qualityPreset;\s*persistSettings\(\);/);
  assert.match(client, /<select id="quality"/);
  assert.match(client, /action: "quality"/);
  assert.match(client, /quality: qualityPreset === "auto" \? "high" : qualityPreset }\);/);
  assert.match(client, /streamMode: streamModeValue\(\)/);
  assert.match(client, /<option value="auto">Auto<\/option>/);
  assert.match(client, /function mediaSocketUrl\(\)/);
  assert.match(client, /role: "media-client"/);
  assert.match(client, /hev1\.1\.6\.L120\.B0/);
  assert.match(client, /media-control/);
  assert.match(client, /fullFrameDiagnostic/);
  assert.match(client, /diagnostic-full-frame/);
  assert.match(client, /clientBuild/);
  assert.match(client, /mediaGeneration/);
  assert.match(client, /<canvas id="screen"><\/canvas><canvas id="sharp-screen"><\/canvas>/);
  assert.match(client, /desiredSharpCrop/);
  assert.match(client, /function viewportForGeneration\(generation\)/);
  assert.match(client, /never render a generation with stale geometry/);
  assert.match(client, /viewport-timeout-full/);
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
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null\)[\s\S]*desktopAwaitingFrame = false[\s\S]*renderDesktopState\(\);/);
  assert.doesNotMatch(client, /sharpZoomEnabled\s*&&\s*pointerMode\s*===\s*"desktop"/);
  assert.match(client, /function requestWssMediaStart\(\) \{\s*if \(usingWssMedia\) return;\s*if \(!mediaEpoch\)[\s\S]*ensureMainCanvas\(\);/);
  assert.match(client, /function requestWssMediaStart\(\)[\s\S]*stage\.classList\.remove\("native-video-input"\)/);
  assert.match(client, /function recoverNetworkRoute\(reason\)/);
  assert.match(client, /addEventListener\("online", \(\) => recoverNetworkRoute\("online"\)\)/);
  assert.match(client, /function probeNetworkRouteChange\(\)/);
  assert.match(client, /routeConnection\?\.addEventListener\("change", probeNetworkRouteChange\)/);
  assert.doesNotMatch(client, /routeConnection\?\.addEventListener\("change", \(\) => recoverNetworkRoute/);
  assert.match(client, /4_000/);
  assert.match(client, /2_000/);
  assert.doesNotMatch(client, /localStorage\.setItem\([^\n]*codec/);
  assert.match(client, /const due = usingWssMedia \? now : latencyGuard\.expectedWall\(ts\)/);
  assert.match(client, /wssLatestFrameTrace/);
  assert.match(client, /wssPresentedFrameTraces/);
  assert.match(client, /frameTraceHistory/);
  assert.match(client, /wssPresentedSequence\s*=\s*metadata\.sequence\s*>>>\s*0/);
  assert.doesNotMatch(client, /wssPresentedSequence\s*=\s*Math\.max\(/);
  assert.match(client, /armPresentedIdentityHistory\(frameCount = 16\)/);
  assert.match(client, /wssPresentedIdentityFramesRemaining > 0\s*\?\s*readTimestampPattern\(presentedCanvas, presentedContext\)/);
  assert.match(client, /wssPresentedIdentityFramesRemaining--/);
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
  assert.match(client, /mediaEpoch = \/\^\[a-f0-9\]\{32\}\$\/\.test\(msg\.mediaEpoch/);
  assert.match(client, /if \(wssSelected\) requestWssMediaStart\(\);\s*else startPeer\(peerGeneration\);/);
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
  assert.match(client, /sendClientLog\("info", "hybrid-frame-activity"/);
  assert.match(client, /sendClientLog\("warn", "wss-recovery-reset"/);
  assert.match(client, /client-health[\s\S]*hybrid:\s*activeWssHybridCache\(\)\.snapshot\(\)/);
  assert.match(client, /sendClientLog\("error", "decoder-decode-throw", \{\s*message:/);
  assert.match(client, /function drawFrame\(frame[\s\S]*drawBaseThenHybrid/);
  assert.match(client, /stopWssMedia\(\)[\s\S]*clearWssHybridCaches\(\)/);
  assert.match(client, /resyncWssDecoder\(reason\)[\s\S]*clearWssHybridCaches\(\)/);
  assert.match(client, /wssMedia:\s*\{[\s\S]*hybrid:\s*activeWssHybridCache\(\)\.snapshot\(\)/);
});

test("peer departure recycles signaling before another peer offer", () => {
  const client = readClientSource();
  assert.match(client, /case "peer-left":\s*recycleSignaling\("peer-left"\);\s*restartPeer\("peer-left", false\);/);
  assert.match(client, /function recycleSignaling\(reason\)/);
});

test("intentional signaling recycle retains its initiator before invalidating the old epoch", () => {
  const client = readClientSource();
  const recycle = client.match(/function recycleSignaling\(reason\) \{([\s\S]*?)\n\}/)?.[1] || "";
  assert.match(recycle, /createSignalingRecycleDiagnostic\(\{/);
  assert.match(recycle, /reason,[\s\S]*signalingGeneration,[\s\S]*signalingRegistered,/);
  assert.match(recycle, /socketReadyState:\s*ws\?\.readyState\s*\?\?\s*null/);
  assert.match(recycle, /usingWssMedia,[\s\S]*mediaSocketOpen:/);
  assert.match(recycle, /mediaGeneration:[\s\S]*reconstructed:[\s\S]*presented:[\s\S]*receivedPackets:/);
  assert.match(recycle, /lastSignalingRecycle\s*=\s*\{\s*\.\.\.diagnostic,\s*atMs:\s*Date\.now\(\)\s*\}/);
  const emittedAt = recycle.indexOf('sendClientLog("warn", "signaling-recycle", diagnostic)');
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
  assert.match(client, /function sendInput\(data, channel\) \{\s*if \(desktopInputState !== "enabled"\) return false;/);
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
  assert.match(client, /const sourceTransition = message\.state === "active" && \["desktop-switched", "capture-recovered", "session-changed", "agent-restarted"\]\.includes\(message\.reason\)/);
  assert.doesNotMatch(client, /sourceTransition = message\.state === "active" && \(secureDesktop \|\|/);
  assert.match(client, /if \(monitorTransition && alreadyRendering\) \{[\s\S]*desktopAwaitingFrame = true;[\s\S]*suppressDesktopTransitionOverlay = true;[\s\S]*if \(!usingWssMedia\) \{[\s\S]*restartPeerInNewEpoch\(\{ recycleSignaling, restartPeer \}, "monitor-transition"\);[\s\S]*startFirstFrameHandshake\("monitor-transition", true\);[\s\S]*return;\s*\}/);
  assert.match(client, /if \(sourceTransition && alreadyRendering\) \{[\s\S]*desktopAwaitingFrame = true;[\s\S]*startFirstFrameHandshake\("desktop-source-transition", true\);[\s\S]*return;\s*\}/);
  assert.match(client, /if \(!usingWssMedia\) \{[\s\S]*restartPeerInNewEpoch\(\{ recycleSignaling, restartPeer \}, "desktop-source-transition"\);[\s\S]*return;/);
  assert.match(client, /if \(workerPresented\) markFirstFramePresented\(\)/);
  assert.match(client, /firstFrameHandshake\.requireAdmission && !usingWssMedia && useWebCodecs[\s\S]*resyncPipeline\("desktop-transition-admitted"\)/);
  assert.match(client, /if \(!usingWssMedia && firstFrameHandshake\.requireAdmission\) \{[\s\S]*if \(!firstFrameHandshake\.admitted\)[\s\S]*if \(transitionNeedsKeyframe\)[\s\S]*if \(!isKey\)/);
  assert.match(client, /function sendInput\(data, channel\) \{[\s\S]*if \(!desktopAvailable \|\| desktopAwaitingFrame\) return false;/);
  assert.match(client, /firstFrameHandshake\.requireAdmission = forceNew \|\| reason === "desktop-source-transition"/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null\)[\s\S]*requireAdmission && !firstFrameHandshake\.admitted[\s\S]*return false/);
  assert.match(client, /presentedGeneration !== firstFrameHandshake\.generation[\s\S]*return false/);
  assert.match(client, /function markFirstFramePresented\(presentedGeneration = null\)[\s\S]*desktopAwaitingFrame = false/);
});

test("monitor control uses the signaling epoch and drops stale confirmations", () => {
  const client = readClientSource();
  assert.match(client, /import \{ monitorRequestAfterRestart, monitorRequestReadiness, restartPeerInNewEpoch \} from "\/monitor-selection\.js"/);
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
  const handler = client.match(/function handleDesktopState\(message\) \{[\s\S]*?\n\}/)?.[0] || "";
  assert.match(client, /let desktopGeneration = null/);
  assert.match(handler, /const desktopGenerationChanged = Number\.isInteger\(message\.generation\)[\s\S]*message\.generation !== desktopGeneration/);
  assert.match(handler, /desktopGeneration = message\.generation/);
  assert.match(handler, /startFirstFrameHandshake\("desktop-state", desktopGenerationChanged\)/);
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
  const client = readClientSource();
  const begin = client.indexOf("// BEGIN secure-composition-gate");
  const end = client.indexOf("// END secure-composition-gate");
  assert.ok(begin >= 0 && end > begin);
  const factory = new Function(`${client.slice(begin, end)}\nreturn createCompositionCommitGate;`)();
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

  assert.match(client, /if \(desktopKind === "secure" && secureCompositionCommit\.consume\(event\.data \?\? keyboardInput\.value\)\) return/);
  assert.match(client, /compositionend", event =>[\s\S]*secureCompositionCommit\.stage\(event\.data \?\? keyboardInput\.value\)/);
});
